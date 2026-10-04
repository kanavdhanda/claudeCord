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
mod disk;
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
    /// How often changed state is saved.
    pub save_every: Duration,
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
            save_every: Duration::from_millis(50),
            tick_every: Duration::from_secs(5),
            rollover_every: Some(Duration::from_secs(3600)),
            hot_window: Duration::from_secs(14 * 24 * 3600),
            backup_every: Some(Duration::from_secs(6 * 3600)),
            oauth: None,
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

/// Something to run against the core: it gets the core and the time and returns the effects to carry out.
pub(crate) type Job = Box<dyn FnOnce(&mut HubCore, i64) -> Vec<crate::hub::Effect> + Send>;

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
    },
    /// A heartbeat answer arrived, so the device is alive.
    Alive {
        node: String,
    },
    /// Run something against the core and the clock (used by the chat bridge and the website).
    Call(Job),
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
            let _ = tx.send(r);
            fx
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
}

impl Hub {
    /// A handle other parts can use to reach the core.
    pub fn handle(&self) -> HubHandle {
        HubHandle {
            to_actor: self.to_actor.clone(),
        }
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
            let _ = tx.send(r);
            fx
        }));
        self.to_actor.send(job).await.ok()?;
        rx.await.ok()
    }

    /// Every machine the hub knows, with whether it is connected and when it was last heard from.
    pub async fn devices(&self) -> Vec<crate::hub::DeviceRow> {
        self.call(|c, _| (c.devices(), vec![]))
            .await
            .unwrap_or_default()
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
    /// Dashboard sign-ins by session id (see `login`).
    pub(crate) sessions: login::Sessions,
}

/// Starts the hub: opens nothing on disk itself (the caller gives it a store and a core), binds the port, starts the
/// actor and the web server, and returns a handle.
pub async fn start(cfg: Config, core: HubCore, store: Store) -> std::io::Result<Hub> {
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    let (to_actor, from_world) = mpsc::channel(4096);
    let (chat_tx, _) = broadcast::channel(1024);
    let reader = Arc::new(Mutex::new(store.fork()));
    // Starting counts as up (and a crash since the last heartbeat counts as down until now).
    let uptime = store.fork().zip(store.fork()).map(|(log, beat)| {
        crate::uptime::recover(&log, now_ms());
        (log, crate::uptime::heartbeat(beat))
    });
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
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = Router::new()
        .route(NODE_CONNECT_PATH, get(connect))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(health::metrics))
        .route("/api/v1/uptime", get(health::uptime))
        .route("/", get(web::index))
        .route("/app.js", get(web::script))
        .route("/app.css", get(web::style))
        .route("/api/v1/state", get(web::state))
        .route("/api/v1/history", get(web::history))
        .route("/auth/login", get(login::start))
        .route("/auth/callback", get(login::callback))
        .route("/auth/me", get(login::me))
        .route("/auth/logout", axum::routing::post(login::logout))
        .with_state(state);
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
    Ok(Hub {
        addr,
        to_actor,
        chat: chat_tx,
        stop_server: Some(stop_tx),
        server: Some(server),
        uptime,
    })
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
    let conn = st
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = st.cfg.clone();
    let to_actor = st.to_actor.clone();
    ws.max_message_size(cfg.max_frame)
        .max_frame_size(cfg.max_frame)
        .on_upgrade(move |socket| session::run(socket, node, conn, to_actor, cfg))
        .into_response()
}

pub(crate) use crate::now_ms;
