//! The connection from a machine to the hub. It only ever dials out, so it works behind a home router, a company
//! firewall or any NAT without opening a port, and it goes through a web proxy when the machine has to use one. It
//! connects, says hello, keeps the connection alive, and when it drops it connects again after a growing, randomised
//! pause, so that a hub restart does not bring every machine back in the same instant.
//!
//! A machine that was asleep reconnects at once when it wakes: a jump in the clock is taken as a wake-up, and any local
//! activity (`Link::nudge`) cuts a waiting pause short. Frames to send wait in a bounded queue while the connection is
//! down. Nothing is lost by a drop: the hub keeps what is waiting, and the owner of this link registers its agents
//! again on every `Up`.

use crate::protocol::{HubFrame, NodeFrame};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// What the link tells its owner. (One variant is a whole frame, which is much larger than the others; these are made one at a time, so that is fine.)
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum LinkEvent {
    /// Connected and welcomed. Register agents now.
    Up,
    /// The connection ended. The link is already trying to get it back.
    Down,
    Frame(HubFrame),
}

/// Timing and routing choices. Tests shorten the times.
#[derive(Clone)]
pub struct LinkOpts {
    pub ping_every: Duration,
    pub connect_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// A web proxy to go through, like `http://user:pass@proxy:3128`. None means look at the standard environment
    /// variables (HTTPS_PROXY, HTTP_PROXY, ALL_PROXY, NO_PROXY).
    pub proxy: Option<String>,
}

impl Default for LinkOpts {
    /// 20 s heartbeat, 10 s to connect, retries from 1 s up to 60 s, proxy from the environment.
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            proxy: None,
        }
    }
}

/// The sending end of a link.
#[derive(Clone)]
pub struct Link {
    out: mpsc::Sender<NodeFrame>,
    wake: Arc<Notify>,
}

impl Link {
    /// Queues a frame to the hub. Waits if the queue is full, which pushes back on whoever is producing too fast.
    pub async fn send(&self, frame: NodeFrame) {
        let _ = self.out.send(frame).await;
    }

    /// Says the machine is active right now (a person is typing, an agent is working). If the link is waiting out a
    /// pause before trying again, it tries at once instead. This is how a laptop that just woke up gets back quickly.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }
}

/// Starts the link in the background. Events arrive on `events`. The link runs until the receiver is dropped.
pub fn spawn(
    url: String,
    token: String,
    node: String,
    events: mpsc::Sender<LinkEvent>,
    opts: LinkOpts,
) -> Link {
    let (out, outbox) = mpsc::channel(1024);
    let wake = Arc::new(Notify::new());
    tokio::spawn(run(url, token, node, events, outbox, opts, wake.clone()));
    Link { out, wake }
}

/// A jump in the wall clock this large between two looks means the machine was suspended in between.
const RESUME_GAP: Duration = Duration::from_secs(15);

/// Whether the clock jumped far enough between `before` and `now` to mean the machine was asleep.
pub fn resumed_from_sleep(before: SystemTime, now: SystemTime, expected: Duration) -> bool {
    now.duration_since(before)
        .is_ok_and(|gap| gap > expected + RESUME_GAP)
}

/// Most frames kept waiting for the hub to confirm them. Past this the oldest is dropped (and logged), so a machine that cannot reach the
/// hub for a very long time cannot grow without limit.
const MAX_UNACKED: usize = 10_000;

/// What this machine has sent and the hub has not confirmed. Each frame is numbered under an epoch chosen at start-up, kept until the hub
/// acknowledges it (which it does only once the change is on disk), and sent again after a reconnect. The hub remembers the last number it
/// took, so a frame that arrives twice is taken once. If this process itself restarts, a new epoch begins and what was unconfirmed is lost
/// with it.
struct Unacked {
    epoch: u64,
    next: u64,
    frames: std::collections::VecDeque<(u64, String)>,
}

impl Unacked {
    fn new() -> Self {
        let mut r = [0u8; 8];
        let _ = getrandom::fill(&mut r);
        // Never zero, so "no epoch" can never be mistaken for one.
        Self {
            epoch: u64::from_le_bytes(r) | 1,
            next: 1,
            frames: Default::default(),
        }
    }

    /// Numbers a frame, remembers it, and returns the text to send.
    fn stamp(&mut self, frame: &NodeFrame) -> String {
        let n = self.next;
        self.next += 1;
        let mut v = serde_json::to_value(frame).expect("plain data");
        if let Some(o) = v.as_object_mut() {
            o.insert("e".into(), self.epoch.into());
            o.insert("n".into(), n.into());
        }
        let text = v.to_string();
        if self.frames.len() >= MAX_UNACKED {
            self.frames.pop_front();
            crate::warn!(
                "link",
                "the hub has not confirmed {MAX_UNACKED} frames; dropped the oldest"
            );
        }
        self.frames.push_back((n, text.clone()));
        text
    }

    /// The hub has everything up to `n`.
    fn ack(&mut self, n: u64) {
        while self.frames.front().is_some_and(|(k, _)| *k <= n) {
            self.frames.pop_front();
        }
    }
}

/// The reconnect loop.
async fn run(
    url: String,
    token: String,
    node: String,
    events: mpsc::Sender<LinkEvent>,
    mut outbox: mpsc::Receiver<NodeFrame>,
    opts: LinkOpts,
    wake: Arc<Notify>,
) {
    let mut failures = 0u32;
    let mut unacked = Unacked::new();
    let mut said = String::new();
    loop {
        let started = tokio::time::Instant::now();
        let conn = connect_detailed(&url, &token, &opts).await;
        // Say why once, not at every retry: a refused login would otherwise fail in silence for ever.
        if let Err(e) = &conn
            && *e != said
        {
            said = e.clone();
            if e.contains("401") {
                crate::error!(
                    "link",
                    "the hub does not recognise this machine's login (removed on the dashboard?). Run `claudecord` to sign in again"
                );
            } else {
                crate::warn!("link", "cannot connect to the hub: {e}");
            }
        }
        if let Ok(ws) = conn {
            said.clear();
            let replaced = session(ws, &node, &events, &mut outbox, &mut unacked, &opts).await;
            if events.send(LinkEvent::Down).await.is_err() {
                return;
            }
            // A connection that lasted a while was a good one, so start the pauses from the beginning again.
            if started.elapsed() > Duration::from_secs(10).min(opts.backoff_max) {
                failures = 0;
            }
            if replaced {
                // Another process on this machine took the connection. Do not fight it.
                failures = failures.max(4);
            }
        }
        failures = failures.saturating_add(1);
        if sleep_or_wake(pause(failures, &opts), &wake, &mut outbox, &mut unacked).await {
            // Woken by activity or by the machine waking up: try straight away and start the pauses over.
            failures = 0;
        }
        if events.is_closed() {
            return;
        }
    }
}

/// Waits for `total`, in short steps. Returns true if it was cut short by a nudge or by the machine waking from sleep. While waiting, frames
/// the machine wants to send are taken from the queue and kept, so a hub that cannot be reached never backs up into the rest of the program.
async fn sleep_or_wake(
    total: Duration,
    wake: &Notify,
    outbox: &mut mpsc::Receiver<NodeFrame>,
    unacked: &mut Unacked,
) -> bool {
    let step = Duration::from_millis(500).min(total);
    let mut waited = Duration::ZERO;
    let mut last = SystemTime::now();
    // One clock for the whole wait, so a stream of frames cannot keep restarting the sleep and never let it end.
    let mut clock = tokio::time::interval(step);
    clock.tick().await;
    while waited < total {
        tokio::select! {
            _ = wake.notified() => return true,
            frame = outbox.recv() => {
                if let Some(f) = frame {
                    unacked.stamp(&f);
                }
                continue;
            }
            _ = clock.tick() => {}
        }
        let now = SystemTime::now();
        if resumed_from_sleep(last, now, step) {
            return true;
        }
        last = now;
        waited += step;
    }
    false
}

/// How long to wait before attempt number `failures`: doubles each time up to a limit, with a random share so many
/// machines do not retry together ("full jitter").
fn pause(failures: u32, o: &LinkOpts) -> Duration {
    let cap = o
        .backoff_min
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(o.backoff_max);
    let mut r = [0u8; 4];
    let _ = getrandom::fill(&mut r);
    let frac = u32::from_le_bytes(r) as f64 / u32::MAX as f64;
    o.backoff_min.max(cap.mul_f64(frac))
}

pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// Opens one connection (through a proxy if one applies), or gives up after the timeout.
pub async fn connect(url: &str, token: &str, opts: &LinkOpts) -> Option<Ws> {
    connect_detailed(url, token, opts).await.ok()
}

/// Like `connect`, but says what went wrong. Used by `doctor`.
pub async fn connect_detailed(url: &str, token: &str, opts: &LinkOpts) -> Result<Ws, String> {
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("bad address: {e}"))?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "token has odd characters".to_string())?,
    );
    let uri = req.uri().clone();
    let host = uri.host().ok_or("address has no host")?.to_string();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let proxy = opts.proxy.clone().or_else(|| proxy_from_env(tls, &host));
    let work = async {
        let stream = match &proxy {
            Some(p) => tunnel(p, &host, port)
                .await
                .map_err(|e| format!("proxy: {e}"))?,
            None => TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?,
        };
        let _ = stream.set_nodelay(true);
        tokio_tungstenite::client_async_tls_with_config(req, stream, None, None)
            .await
            .map(|(ws, _)| ws)
            .map_err(|e| format!("handshake: {e}"))
    };
    tokio::time::timeout(opts.connect_timeout, work)
        .await
        .map_err(|_| "timed out".to_string())?
}

/// The proxy the environment asks for, for a destination host, honouring NO_PROXY.
fn proxy_from_env(tls: bool, host: &str) -> Option<String> {
    let get = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
    };
    if no_proxy_matches(&get(&["NO_PROXY", "no_proxy"]).unwrap_or_default(), host) {
        return None;
    }
    if tls {
        get(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])
    } else {
        get(&["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"])
    }
}

/// Whether a NO_PROXY list (comma separated names, `*` for everything, leading dots allowed) covers a host.
pub fn no_proxy_matches(list: &str, host: &str) -> bool {
    list.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .any(|e| {
            let e = e.trim_start_matches('.');
            e == "*" || host == e || host.ends_with(&format!(".{e}"))
        })
}

/// Opens a tunnel to `host:port` through an HTTP proxy with the CONNECT method. Handles `user:pass@` in the proxy address.
async fn tunnel(proxy: &str, host: &str, port: u16) -> Result<TcpStream, String> {
    let rest = proxy
        .split("://")
        .last()
        .unwrap_or(proxy)
        .trim_end_matches('/');
    let (auth, addr) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let addr = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:80")
    };
    let mut s = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("cannot reach proxy {addr}: {e}"))?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(a) = auth {
        use base64::Engine;
        req.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(a)
        ));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    // Read the reply up to the blank line, one byte at a time so nothing that follows is swallowed.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || s.read(&mut b).await.map_err(|e| e.to_string())? == 0 {
            return Err("proxy closed the connection".into());
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(s)
    } else {
        Err(format!(
            "proxy refused: {}",
            status.lines().next().unwrap_or("")
        ))
    }
}

/// Runs one connection until it ends. Returns true if the hub said it was replaced by another connection.
async fn session(
    ws: Ws,
    node: &str,
    events: &mpsc::Sender<LinkEvent>,
    outbox: &mut mpsc::Receiver<NodeFrame>,
    unacked: &mut Unacked,
    opts: &LinkOpts,
) -> bool {
    let (mut tx, mut rx) = ws.split();
    let hello = NodeFrame::Hello {
        node_name: node.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        features: vec!["hub-spawn".into()],
    };
    if tx
        .send(Message::Text(
            serde_json::to_string(&hello).expect("plain data").into(),
        ))
        .await
        .is_err()
    {
        return false;
    }
    // Everything the hub has not confirmed is sent again, in order, ahead of anything new. The hub takes each numbered frame once.
    for (_, text) in &unacked.frames {
        if tx.send(Message::Text(text.clone().into())).await.is_err() {
            return false;
        }
    }
    let mut ping = tokio::time::interval(opts.ping_every);
    ping.tick().await;
    let mut watch = tokio::time::interval(Duration::from_secs(1));
    let mut last_watch = SystemTime::now();
    let mut last_seen = tokio::time::Instant::now();
    let mut replaced = false;
    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                None | Some(Err(_)) => return replaced,
                Some(Ok(m)) => {
                    last_seen = tokio::time::Instant::now();
                    match m {
                        Message::Text(t) => match HubFrame::parse(t.as_str()) {
                            Some(HubFrame::Welcome { .. }) => {
                                if events.send(LinkEvent::Up).await.is_err() { return replaced; }
                            }
                            Some(HubFrame::Ack { n }) => unacked.ack(n),
                            Some(HubFrame::Error { message }) if message.contains("replaced") => replaced = true,
                            Some(f) => {
                                if events.send(LinkEvent::Frame(f)).await.is_err() { return replaced; }
                            }
                            None => {}
                        },
                        Message::Close(_) => return replaced,
                        _ => {}
                    }
                }
            },
            frame = outbox.recv() => match frame {
                None => return replaced,
                Some(f) => {
                    let text = unacked.stamp(&f);
                    if tx.send(Message::Text(text.into())).await.is_err() { return replaced; }
                }
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > opts.ping_every * 2 { return replaced; }
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() { return replaced; }
            }
            _ = watch.tick() => {
                // The machine was asleep: the old connection is certainly dead, so do not wait to find out.
                let now = SystemTime::now();
                if resumed_from_sleep(last_watch, now, Duration::from_secs(1)) { return replaced; }
                last_watch = now;
            }
        }
    }
}
