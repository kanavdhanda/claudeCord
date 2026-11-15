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
    /// A file to keep the frames the hub has not confirmed in, so they are still sent if this program restarts first. None keeps them in
    /// memory only.
    pub outbox: Option<std::path::PathBuf>,
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
            outbox: None,
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
    /// Creates a link backed directly by an outbound `NodeFrame` channel.
    pub fn from_sender(out: mpsc::Sender<NodeFrame>) -> Self {
        Self {
            out,
            wake: Arc::new(Notify::new()),
        }
    }

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

/// What a machine shows the hub. Its login is a refresh token, kept for long; what goes on each connection and request is an access token (a
/// JWT) the hub gives in exchange, which runs out after minutes. The refresh token is replaced by the hub each time it is used, and the new one
/// is saved in the machine's config straight away. A hub that has no such exchange (an older one) is given the refresh token itself, as before.
pub struct Auth {
    http: String,
    /// Where the saved config is. The refresh token is read from it each time, so every process on this machine uses the newest one.
    dir: Option<std::path::PathBuf>,
    refresh: std::sync::Mutex<String>,
    access: std::sync::Mutex<Option<(String, i64)>>,
    legacy: std::sync::atomic::AtomicBool,
}

impl Auth {
    /// For the hub at `hub_url` (any form of its address) with this refresh token. With a `dir`, the token is kept in (and read from) the
    /// config saved there.
    pub fn new(hub_url: &str, token: String, dir: Option<std::path::PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            http: super::enroll::http_base(hub_url),
            dir,
            refresh: std::sync::Mutex::new(token),
            access: std::sync::Mutex::new(None),
            legacy: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn refresh_token(&self) -> String {
        use crate::sync::Lock;
        self.dir
            .as_deref()
            .and_then(super::config::Config::load)
            .map(|c| c.token)
            .unwrap_or_else(|| self.refresh.locked().clone())
    }

    /// The token to put in `Authorization: Bearer`: the cached access token while it has more than a minute left, else a new one.
    pub async fn bearer(&self) -> Result<String, String> {
        use crate::sync::Lock;
        use std::sync::atomic::Ordering;
        if self.legacy.load(Ordering::Relaxed) {
            return Ok(self.refresh_token());
        }
        let now = crate::now_ms() / 1000;
        if let Some((t, exp)) = self.access.locked().clone()
            && exp - 60 > now
        {
            return Ok(t);
        }
        let old = self.refresh_token();
        let answer = reqwest::Client::new()
            .post(format!("{}/api/device/refresh", self.http))
            .bearer_auth(&old)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("cannot reach the hub: {}", super::enroll::why(&e)))?;
        match answer.status().as_u16() {
            404 | 405 => {
                // A hub without the exchange: carry on the old way.
                self.legacy.store(true, Ordering::Relaxed);
                Ok(old)
            }
            // Either the login really is not known, or this is a hub that answers unknown paths with 401. The token itself is tried next, and
            // the hub's refusal of that is what the person is told.
            401 => Ok(old),
            s if !(200..300).contains(&s) => Err(format!("the hub answered {s} to a refresh")),
            _ => {
                let v: serde_json::Value = answer.json().await.map_err(|e| e.to_string())?;
                let (Some(access), Some(fresh)) =
                    (v["access_token"].as_str(), v["refresh_token"].as_str())
                else {
                    return Err("the hub's refresh answer was not understood".into());
                };
                *self.refresh.locked() = fresh.to_string();
                if let Some(dir) = &self.dir
                    && let Some(mut cfg) = super::config::Config::load(dir)
                {
                    cfg.token = fresh.to_string();
                    if let Err(e) = cfg.save(dir) {
                        // The old token still works for a minute; if it cannot be replaced on disk the next refresh fails and the person logs in again.
                        crate::warn!("link", "could not save the new login: {e}");
                    }
                }
                let exp = crate::security::jwt::expires_at(access).unwrap_or(now + 600);
                *self.access.locked() = Some((access.to_string(), exp));
                Ok(access.to_string())
            }
        }
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
    spawn_with(Auth::new(&url, token, None), url, node, events, opts)
}

/// Like `spawn`, with the login handled by `auth`.
pub fn spawn_with(
    auth: Arc<Auth>,
    url: String,
    node: String,
    events: mpsc::Sender<LinkEvent>,
    opts: LinkOpts,
) -> Link {
    let (out, outbox) = mpsc::channel(1024);
    let wake = Arc::new(Notify::new());
    tokio::spawn(run(auth, url, node, events, outbox, opts, wake.clone()));
    Link { out, wake }
}

/// After the hub says it is restarting, a machine retries every second or so for this long instead of backing off: the hub is back in a few
/// seconds, and a pause that doubled each time would leave machines waiting a minute for a hub that has been up for fifty seconds of it.
const HUB_RESTART_WINDOW: Duration = Duration::from_secs(90);

/// The pause between attempts while the hub is restarting: the shortest pause plus up to as much again, so machines do not all arrive together.
fn restart_pause(o: &LinkOpts) -> Duration {
    let mut r = [0u8; 4];
    let _ = getrandom::fill(&mut r);
    let frac = u32::from_le_bytes(r) as f64 / u32::MAX as f64;
    o.backoff_min + o.backoff_min.mul_f64(frac)
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

/// How long a frame kept on disk stays worth sending. After a restart, a say or a status from this long ago would only confuse.
const OUTBOX_KEEP_MS: i64 = 6 * 3_600_000;

/// The journal is rewritten (what was confirmed dropped) once it has grown by this much.
const OUTBOX_COMPACT_AT: usize = 512 * 1024;

/// What this machine has sent and the hub has not confirmed. Each frame is numbered under an epoch, kept until the hub acknowledges it (which
/// it does only once the change is on disk), and sent again after a reconnect. The hub remembers the last number it took under each epoch, so a
/// frame that arrives twice is taken once.
///
/// With a file to keep it in (`LinkOpts::outbox`) the epoch, the numbers and the unconfirmed frames survive a restart of this program: the
/// next start carries on under the same epoch, so what the hub already took is ignored and what it did not is sent. The file belongs to one
/// hub (a machine moved to another hub starts afresh), holds nothing older than `OUTBOX_KEEP_MS`, and is private to the user.
struct Unacked {
    epoch: u64,
    next: u64,
    frames: std::collections::VecDeque<(u64, String)>,
    journal: Option<Journal>,
}

/// The file behind `Unacked`: a header line, then one line per frame sent (`{"f":[n,at,text]}`) or confirmed (`{"a":n}`), and, after a rewrite,
/// where the numbering had got to (`{"next":n}`).
struct Journal {
    path: std::path::PathBuf,
    hub: String,
    written: usize,
    /// A write failed (a full disk, a folder that went away): said once, and the frames are kept in memory as before.
    failed: bool,
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
            journal: None,
        }
    }

    /// The frames kept in `path` for `hub` if there are any worth sending, else a new start. With no path, memory only.
    fn open(path: Option<&std::path::Path>, hub: &str) -> Self {
        let mut me = Self::new();
        let Some(path) = path else {
            return me;
        };
        me.journal = Some(Journal {
            path: path.to_path_buf(),
            hub: hub.to_string(),
            written: 0,
            failed: false,
        });
        if let Ok(text) = std::fs::read_to_string(path) {
            me.replay(&text, hub, crate::now_ms());
        }
        // What was read is written back as one clean file (stale and confirmed frames gone), or a new file begun.
        me.rewrite();
        me
    }

    /// Takes the state from a journal's text, if it is a readable one for this hub.
    fn replay(&mut self, text: &str, hub: &str, now: i64) {
        let mut lines = text.lines();
        let Some(head) = lines
            .next()
            .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        else {
            return;
        };
        let (Some(epoch), true) = (head["epoch"].as_u64(), head["hub"].as_str() == Some(hub))
        else {
            return;
        };
        let mut frames: std::collections::VecDeque<(u64, String)> = Default::default();
        let mut next = 1u64;
        for line in lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                // A line cut short by a crash: whatever came before it is still good.
                break;
            };
            if let Some(f) = v["f"].as_array() {
                if let (Some(n), Some(at), Some(t)) = (
                    f.first().and_then(|x| x.as_u64()),
                    f.get(1).and_then(|x| x.as_i64()),
                    f.get(2).and_then(|x| x.as_str()),
                ) {
                    next = next.max(n + 1);
                    if now - at <= OUTBOX_KEEP_MS {
                        frames.push_back((n, t.to_string()));
                    }
                }
            } else if let Some(n) = v["a"].as_u64() {
                while frames.front().is_some_and(|(k, _)| *k <= n) {
                    frames.pop_front();
                }
            } else if let Some(n) = v["next"].as_u64() {
                next = next.max(n);
            }
        }
        while frames.len() > MAX_UNACKED {
            frames.pop_front();
        }
        self.epoch = epoch;
        self.next = next;
        self.frames = frames;
    }

    /// Writes the whole state as a fresh journal, replacing the file in one step.
    fn rewrite(&mut self) {
        let Some(j) = self.journal.as_mut() else {
            return;
        };
        let now = crate::now_ms();
        let mut out = serde_json::json!({"v": 1, "hub": j.hub, "epoch": self.epoch}).to_string();
        out.push('\n');
        out.push_str(&serde_json::json!({"next": self.next}).to_string());
        out.push('\n');
        for (n, t) in &self.frames {
            out.push_str(&serde_json::json!({"f": [n, now, t]}).to_string());
            out.push('\n');
        }
        let tmp = j.path.with_extension("tmp");
        let ok = write_private(&tmp, out.as_bytes(), false).is_ok()
            && std::fs::rename(&tmp, &j.path).is_ok();
        j.written = if ok { out.len() } else { j.written };
        Self::note_failure(j, ok);
    }

    /// Adds one line to the journal.
    fn append(&mut self, line: serde_json::Value) {
        let Some(j) = self.journal.as_mut() else {
            return;
        };
        let mut text = line.to_string();
        text.push('\n');
        let ok = write_private(&j.path, text.as_bytes(), true).is_ok();
        if ok {
            j.written += text.len();
        }
        Self::note_failure(j, ok);
        if j.written > OUTBOX_COMPACT_AT {
            self.rewrite();
        }
    }

    fn note_failure(j: &mut Journal, ok: bool) {
        if !ok && !j.failed {
            j.failed = true;
            crate::warn!(
                "link",
                "cannot write {}: frames the hub has not confirmed are kept in memory only, and are lost if this program restarts",
                j.path.display()
            );
        } else if ok {
            j.failed = false;
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
        self.append(serde_json::json!({"f": [n, crate::now_ms(), text]}));
        text
    }

    /// The hub has everything up to `n`.
    fn ack(&mut self, n: u64) {
        let before = self.frames.len();
        while self.frames.front().is_some_and(|(k, _)| *k <= n) {
            self.frames.pop_front();
        }
        if self.frames.len() != before {
            self.append(serde_json::json!({"a": n}));
        }
    }
}

/// Writes (or appends) a file that only this user can read: it holds what agents said to people.
pub(super) fn write_private(
    path: &std::path::Path,
    data: &[u8],
    append: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)?.write_all(data)
}

/// The reconnect loop.
async fn run(
    auth: Arc<Auth>,
    url: String,
    node: String,
    events: mpsc::Sender<LinkEvent>,
    mut outbox: mpsc::Receiver<NodeFrame>,
    opts: LinkOpts,
    wake: Arc<Notify>,
) {
    let mut failures = 0u32;
    let mut unacked = Unacked::open(opts.outbox.as_deref(), &url);
    let mut said = String::new();
    // Set when the hub said it is restarting: until then a failed attempt is retried quickly (see `HUB_RESTART_WINDOW`).
    let mut restarting_until: Option<tokio::time::Instant> = None;
    loop {
        let started = tokio::time::Instant::now();
        let conn = match auth.bearer().await {
            Ok(token) => connect_detailed(&url, &token, &opts).await,
            Err(e) => Err(e),
        };
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
            let mut going_away = false;
            let replaced = session(
                ws,
                &node,
                &events,
                &mut outbox,
                &mut unacked,
                &opts,
                &mut going_away,
            )
            .await;
            if going_away {
                restarting_until = Some(tokio::time::Instant::now() + HUB_RESTART_WINDOW);
            }
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
        let wait = if restarting_until.is_some_and(|t| tokio::time::Instant::now() < t) {
            restart_pause(&opts)
        } else {
            pause(failures, &opts)
        };
        if sleep_or_wake(wait, &wake, &mut outbox, &mut unacked).await {
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
    going_away: &mut bool,
) -> bool {
    let (mut tx, mut rx) = ws.split();
    let hello = NodeFrame::Hello {
        node_name: node.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        // "say-warn": understands a `warn` receipt that comes ahead of the real one. A machine without it takes the first receipt as the answer
        // to a say, so it would never be sent one.
        features: vec!["hub-spawn".into(), "say-warn".into()],
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
        // Biased, and in this order: what the hub sent is read first, so a long stretch of sending never makes a live hub look silent. The stall
        // check comes before more sending for the same reason.
        tokio::select! {
            biased;
            msg = rx.next() => match msg {
                None | Some(Err(_)) => return replaced,
                Some(Ok(m)) => {
                    last_seen = tokio::time::Instant::now();
                    match m {
                        Message::Text(t) => match HubFrame::parse(t.as_str()) {
                            Some(HubFrame::Welcome { node_id, hub_version, min_version, grant_key }) => {
                                if let Some(hv) = &hub_version {
                                    crate::info!(
                                        "device",
                                        "connected to hub (node {node_id}, hub v{hv}, min client v{})",
                                        min_version.as_deref().unwrap_or("any")
                                    );
                                }
                                if events.send(LinkEvent::Up).await.is_err() { return replaced; }
                                if grant_key.is_some()
                                    && events
                                        .send(LinkEvent::Frame(HubFrame::Welcome {
                                            node_id,
                                            hub_version,
                                            min_version,
                                            grant_key,
                                        }))
                                        .await
                                        .is_err()
                                {
                                    return replaced;
                                }
                            }
                            Some(HubFrame::Ack { n }) => unacked.ack(n),
                            Some(HubFrame::Error { message }) if message.contains("replaced") => replaced = true,
                            Some(f) => {
                                if events.send(LinkEvent::Frame(f)).await.is_err() { return replaced; }
                                // Time spent waiting for the owner to take the frame is ours, not the hub's silence.
                                last_seen = tokio::time::Instant::now();
                            }
                            None => {}
                        },
                        Message::Close(frame) => {
                            // The hub said it is restarting: it will be back in moments, so the pauses before retrying stay short.
                            if frame.is_some_and(|f| f.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away) {
                                *going_away = true;
                            }
                            return replaced;
                        }
                        _ => {}
                    }
                }
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > opts.ping_every * 2 { return replaced; }
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() { return replaced; }
            }
            frame = outbox.recv() => match frame {
                None => return replaced,
                Some(f) => {
                    let text = unacked.stamp(&f);
                    if tx.send(Message::Text(text.into())).await.is_err() { return replaced; }
                }
            },
            _ = watch.tick() => {
                // The machine was asleep: the old connection is certainly dead, so do not wait to find out.
                let now = SystemTime::now();
                if resumed_from_sleep(last_watch, now, Duration::from_secs(1)) { return replaced; }
                last_watch = now;
            }
        }
    }
}

#[cfg(test)]
mod outbox_tests {
    use super::*;

    fn frame(t: &str) -> NodeFrame {
        NodeFrame::AgentSay {
            agent_id: "p/a".into(),
            text: t.into(),
            thread: None,
            say_id: None,
        }
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cc-outbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn what_the_hub_has_not_confirmed_survives_a_restart_under_the_same_epoch() {
        let d = dir("restart");
        let path = d.join("outbox.jsonl");
        let mut a = Unacked::open(Some(&path), "wss://hub/a");
        for t in ["one", "two", "three"] {
            a.stamp(&frame(t));
        }
        a.ack(1);
        let epoch = a.epoch;
        drop(a);
        // The program starts again: the same epoch, the two frames still owed, and numbering that carries on (no number is used twice).
        let mut b = Unacked::open(Some(&path), "wss://hub/a");
        assert_eq!(b.epoch, epoch);
        assert_eq!(
            b.frames.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(b.frames[0].1.contains("two") && b.frames[1].1.contains("three"));
        b.stamp(&frame("four"));
        assert_eq!(b.frames.back().unwrap().0, 4);
        // Everything confirmed: nothing owed, but the numbers still do not start over under that epoch.
        b.ack(4);
        drop(b);
        let c = Unacked::open(Some(&path), "wss://hub/a");
        assert!(c.frames.is_empty());
        assert_eq!((c.epoch, c.next), (epoch, 5));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "what agents said is private");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn frames_for_another_hub_or_too_old_or_cut_short_are_not_trusted() {
        let d = dir("trust");
        let path = d.join("outbox.jsonl");
        let mut a = Unacked::open(Some(&path), "wss://hub/a");
        a.stamp(&frame("one"));
        let epoch = a.epoch;
        drop(a);
        // A machine moved to another hub does not send the old hub's frames to the new one.
        let b = Unacked::open(Some(&path), "wss://hub/b");
        assert!(b.frames.is_empty());
        assert_ne!(b.epoch, epoch);
        drop(b);
        // A frame older than the limit is dropped, but its number is not reused.
        let mut text = serde_json::json!({"v": 1, "hub": "h", "epoch": 7}).to_string();
        text.push('\n');
        text.push_str(&serde_json::json!({"f": [1, 0, "old"]}).to_string());
        text.push('\n');
        text.push_str(&serde_json::json!({"f": [2, crate::now_ms(), "fresh"]}).to_string());
        text.push('\n');
        // And a last line cut short by a crash is ignored, what came before it kept.
        text.push_str("{\"f\":[3,");
        std::fs::write(&path, &text).unwrap();
        let c = Unacked::open(Some(&path), "h");
        assert_eq!((c.epoch, c.next), (7, 3));
        assert_eq!(
            c.frames.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![2]
        );
        // Not a journal at all: a fresh start, not a failure.
        std::fs::write(&path, "garbage").unwrap();
        assert!(Unacked::open(Some(&path), "h").frames.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_pause_after_a_hub_restart_stays_short() {
        let o = LinkOpts::default();
        for _ in 0..50 {
            let p = restart_pause(&o);
            assert!(p >= o.backoff_min && p <= o.backoff_min * 2, "{p:?}");
        }
    }
}
