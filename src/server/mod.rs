//! The hub as a running server: one small web server that accepts devices over WebSocket, feeds what they send to the
//! hub core, and carries out what the core asks for. One process, one port. The same code scales from one core (the
//! actor and every connection are tasks on one runtime) to a fleet (see learnings/09).
//!
//! Files:
//! - `actor`   the single task that owns the core and the store, and turns effects into action
//! - `web`     the read-only dashboard page and its two data endpoints
//! - `session` one device's WebSocket: authentication, heartbeat, rate limit, size limits, slow-reader eviction
//!
//! A device connection is a path to deliver messages, not the record of anything. If it drops, nothing is lost: the core
//! keeps what is waiting, and the device simply reconnects and registers again.

pub mod actor;
pub mod app;
pub mod bots;
pub mod demo;
mod disk;
pub mod gateway;
pub mod health;
pub mod login;
pub mod session;
pub mod web;

use crate::hub::{Chat, HubCore};
use crate::protocol::NODE_CONNECT_PATH;
use crate::security::limits::FailureLimiter;
use crate::store::Store;
use crate::sync::Lock;
use axum::{
    Router,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Tunable limits. The defaults suit production. Tests shrink the times.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// How often a device is pinged. A device silent for two intervals is dropped.
    pub ping_every: Duration,
    /// Largest single message accepted from a device, in bytes.
    pub max_frame: usize,
    /// Most bytes allowed to wait for a device that is not reading, before it is dropped.
    pub max_out_bytes: usize,
    /// How often time-based work (reminders, expiry, release of waiting messages) runs.
    pub tick_every: Duration,
    /// How often old history is moved out of the database into compressed files. None never does.
    pub rollover_every: Option<Duration>,
    /// History newer than this stays in the database where it can be searched quickly.
    pub hot_window: Duration,
    /// How often the live database is copied to the bucket (only if a bucket is set up). None never does.
    pub backup_every: Option<Duration>,
    /// "Sign in with Discord" for the dashboard. None leaves only dashboard tokens.
    pub oauth: Option<Oauth>,
    /// The hub's log file, for the dashboard's log view (`/api/v1/logs`). None leaves that route off.
    pub log_path: Option<std::path::PathBuf>,
    /// Most device connections at once. More are refused with "try later" instead of running the hub out of file handles.
    pub max_devices: usize,
    /// Whether a Discord bridge is run with this hub, so readiness includes it.
    pub discord_expected: bool,
    /// The availability target the dashboard measures the error budget against.
    pub uptime_target: f64,
}

/// Discord OAuth2 settings for the dashboard sign-in. `authorize_url` and `api_base` are Discord's, and are changed only by tests.
#[derive(Clone)]
pub struct Oauth {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub api_base: String,
}

impl Default for Config {
    /// Production defaults: a 20 s ping, 1 MiB frames, 4 MiB of backlog per device.
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".parse().expect("valid address"),
            ping_every: Duration::from_secs(20),
            max_frame: 1 << 20,
            max_out_bytes: 4 << 20,
            tick_every: Duration::from_secs(5),
            rollover_every: Some(Duration::from_secs(3600)),
            hot_window: Duration::from_secs(14 * 24 * 3600),
            backup_every: Some(Duration::from_secs(3600)),
            oauth: None,
            log_path: None,
            max_devices: 20_000,
            discord_expected: false,
            uptime_target: 0.999,
        }
    }
}

/// What a device connection is sent: a frame, or an order to close.
pub(crate) enum Out {
    Frame(String),
    Close(u16, &'static str),
}

/// What happens once a job's changes are on disk: the caller gets its answer. Held back until then, so nobody is told "done" about
/// something that a crash could still lose.
pub(crate) type Reply = Box<dyn FnOnce() + Send>;

/// Something to run against the core: it gets the core and the time and returns the effects to carry out, and the reply for the
/// caller. Both wait for the commit.
pub(crate) type Job = Box<dyn FnOnce(&mut HubCore, i64) -> (Vec<crate::hub::Effect>, Reply) + Send>;

/// Something to read from the core. It changes nothing, so it is answered at once and never costs a write.
pub(crate) type ReadJob = Box<dyn FnOnce(&HubCore, i64) + Send>;

/// A job for the actor.
pub(crate) enum Input {
    /// Is this token real? Answers with the device's name.
    Auth {
        token: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Connected {
        node: String,
        conn: u64,
        tx: mpsc::Sender<Out>,
        queued: Arc<std::sync::atomic::AtomicUsize>,
        /// Fired by the actor to end the connection at once, even while the session is stuck writing to a device that stopped reading.
        kill: Arc<tokio::sync::Notify>,
    },
    Disconnected {
        node: String,
        conn: u64,
    },
    Frame {
        node: String,
        text: String,
        /// The machine's epoch and sequence number for this frame, if it numbered it (see `protocol::stamp_of`).
        stamp: Option<(u64, u64)>,
    },
    /// A heartbeat answer arrived, so the device is alive.
    Alive {
        node: String,
    },
    /// Run something against the core and the clock that may change it (used by the chat bridge and the website).
    Call(Job),
    /// Look at the core without changing it.
    Read(ReadJob),
    /// Save and stop, then say when done.
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// A cheap copy of the way to reach a running hub, for the parts that talk to it from outside (the chat bridge, the
/// website). It can run a closure against the core, but cannot shut the hub down.
#[derive(Clone)]
pub struct HubHandle {
    to_actor: mpsc::Sender<Input>,
}

impl HubHandle {
    /// Runs a closure against the core and returns what it returns. Effects it produces are carried out. None if the
    /// hub is gone or the closure failed.
    pub async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut HubCore, i64) -> (R, Vec<crate::hub::Effect>) + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        let job = Input::Call(Box::new(move |core, now| {
            let (r, fx) = f(core, now);
            let reply: Reply = Box::new(move || {
                let _ = tx.send(r);
            });
            (fx, reply)
        }));
        self.to_actor.send(job).await.ok()?;
        rx.await.ok()
    }

    /// Looks at the core and returns what the closure returns. It cannot change anything, so it is answered at once. None if the hub is
    /// gone or the closure failed.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&HubCore, i64) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        let job = Input::Read(Box::new(move |core, now| {
            let _ = tx.send(f(core, now));
        }));
        self.to_actor.send(job).await.ok()?;
        rx.await.ok()
    }
}

/// A handle to a running hub.
pub struct Hub {
    pub addr: SocketAddr,
    pub(crate) to_actor: mpsc::Sender<Input>,
    chat: broadcast::Sender<Chat>,
    stop_server: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    /// The uptime log and the task that keeps the hub's heartbeat (see `crate::uptime`). None for an in-memory store.
    uptime: Option<(Store, crate::uptime::Heartbeat)>,
    /// What the front door needs to serve this hub's devices and dashboard.
    pub(crate) state: AppState,
}

impl Hub {
    /// A handle other parts can use to reach the core.
    pub fn handle(&self) -> HubHandle {
        HubHandle {
            to_actor: self.to_actor.clone(),
        }
    }

    /// The sending end of the chat broadcast, for whoever starts bridges later (each takes its own receiver with `subscribe`).
    pub(crate) fn chat_sender(&self) -> broadcast::Sender<Chat> {
        self.chat.clone()
    }

    /// Everything the core wants shown in chat arrives here. The chat bridge listens on it.
    pub fn chat(&self) -> broadcast::Receiver<Chat> {
        self.chat.subscribe()
    }

    /// Runs a closure against the core and returns what it returns. Effects it produces are carried out.
    pub async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut HubCore, i64) -> (R, Vec<crate::hub::Effect>) + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        let job = Input::Call(Box::new(move |core, now| {
            let (r, fx) = f(core, now);
            let reply: Reply = Box::new(move || {
                let _ = tx.send(r);
            });
            (fx, reply)
        }));
        self.to_actor.send(job).await.ok()?;
        rx.await.ok()
    }

    /// Looks at the core without changing it (see `HubHandle::read`).
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&HubCore, i64) -> R + Send + 'static,
    ) -> Option<R> {
        self.handle().read(f).await
    }

    /// Every machine the hub knows, with whether it is connected and when it was last heard from.
    pub async fn devices(&self) -> Vec<crate::hub::DeviceRow> {
        self.read(|c, _| c.devices()).await.unwrap_or_default()
    }

    /// Stops accepting devices, tells every connected device the hub is going away (so it reconnects at once), saves
    /// state, and waits for all of that to finish.
    pub async fn shutdown(mut self) {
        let (done, wait) = oneshot::channel();
        let _ = self.to_actor.send(Input::Shutdown { done }).await;
        let _ = wait.await;
        if let Some(s) = self.stop_server.take() {
            let _ = s.send(());
        }
        if let Some(h) = self.server.take() {
            let _ = h.await;
        }
        if let Some((log, beat)) = self.uptime.take() {
            drop(beat);
            crate::uptime::stopped(&log, now_ms());
        }
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) to_actor: mpsc::Sender<Input>,
    pub(crate) handle: HubHandle,
    /// A second database connection for the dashboard's reads, so it never queues behind the hub's own writes.
    pub(crate) reader: Arc<Mutex<Option<Store>>>,
    pub(crate) failures: Arc<Mutex<FailureLimiter>>,
    cfg: Config,
    next_conn: Arc<std::sync::atomic::AtomicU64>,
    /// Device connections open right now, for the cap.
    open: Arc<std::sync::atomic::AtomicUsize>,
    /// Dashboard sign-ins by session id (see `login`).
    pub(crate) sessions: login::Sessions,
}

/// Starts the hub: opens nothing on disk itself (the caller gives it a store and a core), binds the port, starts the
/// actor and the web server, and returns a handle.
pub async fn start(cfg: Config, core: HubCore, store: Store) -> std::io::Result<Hub> {
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    let mut hub = spawn_core(cfg, core, store);
    hub.addr = addr;
    let app = Router::new()
        .route(NODE_CONNECT_PATH, get(connect))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(health::metrics))
        .route("/api/v1/uptime", get(health::uptime))
        .route("/api/v1/logs", get(health::logs))
        .route("/", get(web::index))
        .route("/app.js", get(web::script))
        .route("/app.css", get(web::style))
        .route("/api/v1/state", get(web::state))
        .route("/api/v1/history", get(web::history))
        .route("/auth/login", get(login::start))
        .route("/auth/callback", get(login::callback))
        .route("/auth/me", get(login::me))
        .route("/auth/logout", axum::routing::post(login::logout))
        .with_state(hub.state.clone())
        .layer(axum::middleware::from_fn(real_client));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await;
    });
    hub.stop_server = Some(stop_tx);
    hub.server = Some(server);
    Ok(hub)
}

/// Starts a hub's actor, its watchdog and its heartbeat WITHOUT listening on any port: whoever owns the front door (the single-team
/// `start` above, or the multi-team gateway in `control::registry`) takes the returned `Hub`'s `state` and serves it.
pub fn spawn_core(cfg: Config, core: HubCore, store: Store) -> Hub {
    let (to_actor, from_world) = mpsc::channel(4096);
    let (chat_tx, _) = broadcast::channel(1024);
    let reader = Arc::new(Mutex::new(store.fork()));
    // Starting counts as up (and a crash since the last heartbeat counts as down until now).
    let uptime = store.fork().zip(store.fork()).map(|(log, beat)| {
        crate::uptime::recover(&log, now_ms());
        (log, crate::uptime::heartbeat(beat))
    });
    let addr = cfg.bind;
    tokio::spawn(actor::run(
        core,
        store,
        from_world,
        chat_tx.clone(),
        cfg.clone(),
    ));
    let state = AppState {
        to_actor: to_actor.clone(),
        handle: HubHandle {
            to_actor: to_actor.clone(),
        },
        reader,
        failures: Arc::new(Mutex::new(FailureLimiter::new(10, 60_000.0))),
        cfg,
        next_conn: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        open: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    watchdog(HubHandle {
        to_actor: to_actor.clone(),
    });
    Hub {
        addr,
        to_actor,
        chat: chat_tx,
        stop_server: None,
        server: None,
        uptime,
        state,
    }
}

/// Whether a connection comes from this machine or from a private network address, which is where a reverse proxy sits: on the same host, or
/// outside the container the hub runs in (Docker shows the host to it as a private address, not as 127.0.0.1).
fn from_proxy_side(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Behind a reverse proxy every request arrives from the proxy's address, so one person's failed sign-ins would block everybody and the rate
/// limits would be shared. When the direct peer is on the proxy's side, the address the proxy reports in `X-Forwarded-For` (its last entry, the
/// one the proxy itself added) is used instead. Anyone reaching the hub from a public address cannot choose theirs.
pub(crate) async fn real_client(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    if let Some(peer) = peer
        && from_proxy_side(peer.ip())
        && let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|s| s.trim().parse::<std::net::IpAddr>().ok())
    {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, peer.port())));
    }
    next.run(req).await
}

/// The WebSocket door. Checks the caller is not being blocked for repeated failures, checks its token, and only then
/// upgrades the connection. Everything else about the connection happens in `session`.
async fn connect(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let ip = peer.ip().to_string();
    let now = now_ms() as f64;
    if st.failures.locked().blocked(&ip, now) {
        crate::debug!(
            "hub",
            "refused a connection from {ip}: too many failed attempts"
        );
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let (reply, answer) = oneshot::channel();
    let node = if token.is_empty()
        || st
            .to_actor
            .send(Input::Auth { token, reply })
            .await
            .is_err()
    {
        None
    } else {
        answer.await.ok().flatten()
    };
    // A dashboard token never connects a machine.
    let node = node.filter(|n| !n.starts_with(web::WEB_PREFIX));
    let Some(node) = node else {
        crate::warn!(
            "hub",
            "refused a device connection from {ip}: the token is missing or not valid"
        );
        st.failures.locked().fail(&ip, now);
        return StatusCode::UNAUTHORIZED.into_response();
    };
    admit(&st, &ip, node, ws)
}

/// Lets an already authenticated machine in: checks the connection cap, then upgrades the socket and runs the session. Used by the
/// single-team door above and by the multi-team gateway, which checks the token in `control.db` instead.
pub(crate) fn admit(
    st: &AppState,
    ip: &str,
    node: String,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    // Past the cap a device is told to try later, rather than the hub running out of file handles and failing for everyone.
    let slot = OpenSlot::take(&st.open, st.cfg.max_devices);
    let Some(slot) = slot else {
        crate::warn!(
            "hub",
            "refused {node} from {ip}: already at the limit of {} device connections",
            st.cfg.max_devices
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let conn = st
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = st.cfg.clone();
    let to_actor = st.to_actor.clone();
    ws.max_message_size(cfg.max_frame)
        .max_frame_size(cfg.max_frame)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            session::run(socket, node, conn, to_actor, cfg).await
        })
        .into_response()
}

/// One of the hub's device connection places; given back when dropped.
struct OpenSlot(Arc<std::sync::atomic::AtomicUsize>);

impl OpenSlot {
    fn take(open: &Arc<std::sync::atomic::AtomicUsize>, max: usize) -> Option<Self> {
        use std::sync::atomic::Ordering::SeqCst;
        open.try_update(SeqCst, SeqCst, |n| (n < max).then_some(n + 1))
            .ok()?;
        Some(Self(open.clone()))
    }
}

impl Drop for OpenSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Asks the hub's core a trivial question every few seconds and says so in the log when it does not answer in time, because a stuck
/// core looks from outside like every device quietly going silent. Ends when the hub does.
fn watchdog(handle: HubHandle) {
    tokio::spawn(async move {
        let mut stuck = false;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match tokio::time::timeout(Duration::from_secs(5), handle.call(|_, _| ((), vec![])))
                .await
            {
                Ok(Some(())) => {
                    // Only a core that answers tells systemd the hub is alive, so a stuck one gets restarted.
                    crate::notify::alive();
                    if stuck {
                        crate::info!("hub", "the core answers again");
                    }
                    stuck = false;
                }
                Ok(None) => return,
                Err(_) => {
                    if !stuck {
                        crate::error!(
                            "hub",
                            "the core has not answered for 5 seconds; it may be stuck (see /readyz)"
                        );
                    }
                    stuck = true;
                }
            }
        }
    });
}

pub(crate) use crate::now_ms;

#[cfg(test)]
mod proxy_tests {
    use super::from_proxy_side;

    #[test]
    fn only_this_machine_and_private_addresses_may_name_the_client() {
        let yes = [
            "127.0.0.1",
            "10.1.2.3",
            "172.18.0.1",
            "192.168.1.5",
            "::1",
            "fd00::1",
        ];
        let no = ["203.0.113.9", "8.8.8.8", "172.32.0.1", "2001:db8::1"];
        for a in yes {
            assert!(from_proxy_side(a.parse().unwrap()), "{a}");
        }
        for a in no {
            assert!(!from_proxy_side(a.parse().unwrap()), "{a}");
        }
    }
}
