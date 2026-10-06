//! The gateway: the hosted service's one front door. It owns the port, and for every request works out WHICH account it belongs to from the
//! control database (a machine's token, or a dashboard session cookie), then hands it to that account's own hub. Nothing in a request
//! can name another account's hub: the tenant always comes from the credential.
//!
//! What it serves: machines connecting (`/api/v1/node/connect`), "Sign in with Discord" for people (`/auth/*`), the device-code approval
//! that replaces hand-made machine tokens (`/api/device/*`), and the account's own data (`/api/v1/*`).

use super::login::{cookie, identify, page, pct, random_hex, same, set_cookie};
use super::web::{Who, secure, state_json};
use super::{Oauth, admit};
use crate::control::registry::Registry;
use crate::control::seal::KeyProvider;
use crate::control::{Account, Control, Poll};
use crate::protocol::NODE_CONNECT_PATH;
use crate::security::limits::FailureLimiter;
use crate::sync::Lock;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// What the gateway is started with.
pub struct GatewayConfig {
    pub bind: SocketAddr,
    /// The address people use in a browser, such as `https://claudecord.example.com`. Used for links and to decide on Secure cookies.
    pub public_url: String,
    /// The platform's own Discord application, used only to sign people in. None leaves sign-in off.
    pub oauth: Option<Oauth>,
    /// The hub settings every account's hub starts with.
    pub hub: super::Config,
    /// Where old history and backups go (the operator's bucket), if set up.
    pub bucket: Option<crate::store::bucket::Bucket>,
    /// Where Discord is (changed only by tests and `--dev`).
    pub discord: crate::control::registry::DiscordSettings,
    /// Local preview: `/auth/dev` signs anyone in without Discord. Refused unless the service listens on this machine only.
    pub dev: bool,
    /// Discord people who own every account's projects, besides the account's own person. Empty for the real service.
    pub extra_owners: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct Gateway {
    pub(crate) control: Arc<Control>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) keys: Arc<dyn KeyProvider>,
    pub(crate) discord: crate::control::registry::DiscordSettings,
    pub(crate) oauth: Option<Oauth>,
    pub(crate) public_url: String,
    pub(crate) dev: bool,
    /// Wrong codes, bad tokens and failed sign-ins, per address.
    pub(crate) failures: Arc<Mutex<FailureLimiter>>,
    /// Asking for new device codes, per address, so nobody fills the table.
    pub(crate) code_asks: Arc<Mutex<FailureLimiter>>,
    /// Project choices waiting to be made on the dashboard, by short code (see `pick_start`).
    pub(crate) picks: Arc<Mutex<HashMap<String, Pick>>>,
    /// What Discord said lately about the servers a bot is in, by `account/bot`, so the dashboard asking often does not ask Discord as often.
    pub(crate) guild_cache: Arc<Mutex<GuildCache>>,
}

/// The servers of each bot as Discord last said, with when (milliseconds), by `account/bot`.
pub(crate) type GuildCache = HashMap<String, (i64, Vec<serde_json::Value>)>;

/// A machine asking the person to choose, on the dashboard, which project a folder belongs to.
pub(crate) struct Pick {
    tenant: String,
    node: String,
    folder: String,
    /// The project the folder already belongs to on the machine, offered first on the page.
    hint: Option<String>,
    chosen: Option<String>,
    /// What else the page asked for with the project: the agent's name, program and role.
    agent: Option<String>,
    adapter: Option<String>,
    role: Option<String>,
    at: i64,
}

/// How long a pick waits for the person before it is forgotten.
const PICK_TTL_MS: i64 = 60 * 60_000;

/// A running gateway.
pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub control: Arc<Control>,
    pub(crate) registry: Arc<Registry>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl GatewayHandle {
    /// Stops accepting, then saves and stops every account's hub.
    pub async fn shutdown(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
        self.registry.shutdown().await;
    }
}

/// Starts the gateway on `cfg.bind`, keeping every account's files under `data`.
pub async fn start_gateway(
    cfg: GatewayConfig,
    data: std::path::PathBuf,
    control: Arc<Control>,
    keys: Arc<dyn KeyProvider>,
) -> std::io::Result<GatewayHandle> {
    if cfg.dev && !cfg.bind.ip().is_loopback() {
        return Err(std::io::Error::other(
            "the dev sign-in lets anyone in, so it only runs on a loopback address",
        ));
    }
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    let registry = Arc::new(
        Registry::new(
            data,
            control.clone(),
            cfg.hub,
            keys.clone(),
            cfg.discord.clone(),
        )
        .with_bucket(cfg.bucket)
        .with_extra_owners(cfg.extra_owners.clone()),
    );
    let gw = Gateway {
        control: control.clone(),
        registry: registry.clone(),
        keys,
        discord: cfg.discord,
        oauth: cfg.oauth,
        public_url: cfg.public_url.trim_end_matches('/').to_string(),
        dev: cfg.dev,
        failures: Arc::new(Mutex::new(FailureLimiter::new(10, 60_000.0))),
        code_asks: Arc::new(Mutex::new(FailureLimiter::new(20, 60_000.0))),
        picks: Arc::new(Mutex::new(HashMap::new())),
        guild_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = router(gw).layer(axum::middleware::from_fn(super::real_client));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await;
    });
    Ok(GatewayHandle {
        addr,
        control,
        registry,
        stop: Some(stop_tx),
        task: Some(task),
    })
}

/// All the gateway's routes.
pub(crate) fn router(gw: Gateway) -> Router {
    Router::new()
        .route(NODE_CONNECT_PATH, get(connect))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/auth/login", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/dev", get(auth_dev))
        .route("/auth/logout", post(auth_logout))
        .route("/api/device/project/{name}", get(device_project))
        .route("/api/device/pick", post(pick_start))
        .route("/api/device/pick/{code}", get(pick_poll))
        .route("/api/v1/pick/{code}", get(pick_view).post(pick_choose))
        .route("/api/device/code", post(device_code))
        .route("/api/device/token", post(device_token))
        .route("/api/device/lookup", get(device_lookup))
        .route("/api/device/approve", post(device_approve))
        .route("/api/device/deny", post(device_deny))
        .route("/api/v1/me", get(me))
        .route("/api/v1/state", get(state))
        .route("/api/v1/insights", get(insights))
        .route("/api/v1/export", get(export))
        .route("/api/v1/machines", get(machines))
        .route("/api/v1/machines/revoke", post(machine_revoke))
        .merge(crate::server::bots::routes())
        .route("/app.js", get(super::app::script))
        .route("/app.css", get(super::app::style))
        .fallback(super::app::fallback)
        .with_state(gw)
}

pub(crate) fn json_reply(status: StatusCode, v: serde_json::Value) -> Response {
    secure(
        (status, Json(v)).into_response(),
        "application/json",
        "no-store",
    )
}

pub(crate) fn err(status: StatusCode, text: &str) -> Response {
    json_reply(status, json!({"error": text}))
}

fn ip_of(peer: SocketAddr) -> String {
    peer.ip().to_string()
}

/// The account behind the dashboard session cookie, if it is a live session.
pub(crate) fn account_of(gw: &Gateway, headers: &HeaderMap) -> Option<Account> {
    let sid = cookie(headers, "cc_session")?;
    gw.control.session(&sid, crate::now_ms()).ok().flatten()
}

/// A machine's name as allowed: letters, digits, dot, dash and underscore, starting with a letter or digit.
pub(crate) fn valid_node(n: &str) -> bool {
    (1..=40).contains(&n.len())
        && n.as_bytes()[0].is_ascii_alphanumeric()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

// ----- machines connecting -----

/// A machine's WebSocket: its token is looked up in the control database, which says whose machine it is.
async fn connect(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (ip, now) = (ip_of(peer), crate::now_ms() as f64);
    if gw.failures.locked().blocked(&ip, now) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let found = if token.is_empty() {
        None
    } else {
        gw.control.machine_for_token(token).ok().flatten()
    };
    let Some((tenant, node)) = found else {
        crate::warn!(
            "gateway",
            "refused a machine from {ip}: the token is missing or not valid"
        );
        gw.failures.locked().fail(&ip, now);
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match gw.registry.hub_of(&tenant) {
        Ok(Some(t)) => admit(&t.state, &ip, node, ws),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// `GET /api/device/project/{name}`: asks, with a machine's own token, whether a project of its account has a Discord channel yet, so that
/// `claudecord start` can send the person to the dashboard's setup page only when it is needed. It says nothing else about the account.
async fn device_project(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (tenant, _) = match machine_auth(&gw, &headers, &ip_of(peer), crate::now_ms() as f64) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    if !crate::protocol::is_slug(&name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let target = gw.control.target(&tenant, &name).ok().flatten();
    let problem = match &target {
        Some(t) => super::bots::project_problem(&gw, &tenant, t).await,
        None => None,
    };
    json_reply(
        StatusCode::OK,
        json!({ "placed": target.is_some(), "problem": problem }),
    )
}

/// The machine behind a request's token: (account, machine name). A wrong or missing token counts against the caller's address.
fn machine_auth(
    gw: &Gateway,
    headers: &HeaderMap,
    ip: &str,
    now: f64,
) -> Result<(String, String), Box<Response>> {
    if gw.failures.locked().blocked(ip, now) {
        return Err(Box::new(StatusCode::TOO_MANY_REQUESTS.into_response()));
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let found = if token.is_empty() {
        None
    } else {
        gw.control.machine_for_token(token).ok().flatten()
    };
    found.ok_or_else(|| {
        gw.failures.locked().fail(ip, now);
        Box::new(StatusCode::UNAUTHORIZED.into_response())
    })
}

#[derive(Deserialize)]
struct PickStart {
    folder: String,
    project: Option<String>,
}

/// `POST /api/device/pick`: `claudecord start` in a folder that belongs to no project yet asks for a short code. The person opens
/// `/pick?code=...` on the dashboard, chooses or names the project there, and the machine collects the answer (`pick_poll`). The choice is
/// only ever made on the dashboard, by a signed-in person of the same account.
async fn pick_start(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(b): Json<PickStart>,
) -> Response {
    let (ip, now) = (ip_of(peer), crate::now_ms());
    let (tenant, node) = match machine_auth(&gw, &headers, &ip, now as f64) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    let folder: String = b
        .folder
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    let hint = b.project.filter(|p| crate::protocol::is_slug(p));
    let mut picks = gw.picks.locked();
    picks.retain(|_, p| now - p.at < PICK_TTL_MS);
    if picks.len() >= 500 {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many waiting, try again in a few minutes",
        );
    }
    let code = random_hex().chars().take(12).collect::<String>();
    picks.insert(
        code.clone(),
        Pick {
            tenant,
            node,
            folder,
            hint,
            chosen: None,
            agent: None,
            adapter: None,
            role: None,
            at: now,
        },
    );
    json_reply(StatusCode::OK, json!({ "code": code }))
}

/// `GET /api/device/pick/{code}`: the machine that asked collects the answer: null until the person has chosen.
async fn pick_poll(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let (ip, now) = (ip_of(peer), crate::now_ms());
    let (tenant, node) = match machine_auth(&gw, &headers, &ip, now as f64) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    let mut picks = gw.picks.locked();
    match picks.get(&code) {
        Some(p) if p.tenant == tenant && p.node == node && now - p.at < PICK_TTL_MS => {
            let (chosen, agent, adapter, role) = (
                p.chosen.clone(),
                p.agent.clone(),
                p.adapter.clone(),
                p.role.clone(),
            );
            if chosen.is_some() {
                picks.remove(&code);
            }
            json_reply(
                StatusCode::OK,
                json!({ "chosen": chosen, "agent": agent, "adapter": adapter, "role": role }),
            )
        }
        _ => err(
            StatusCode::NOT_FOUND,
            "that choice expired; run the command again",
        ),
    }
}

/// `GET /api/v1/pick/{code}`: the dashboard page asks what is being chosen (the folder and the machine), for the signed-in owner only.
async fn pick_view(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let now = crate::now_ms();
    match gw.picks.locked().get(&code) {
        Some(p) if p.tenant == a.id && now - p.at < PICK_TTL_MS => json_reply(
            StatusCode::OK,
            json!({ "folder": p.folder, "node": p.node, "project": p.hint }),
        ),
        _ => err(
            StatusCode::NOT_FOUND,
            "that choice expired; run the command again",
        ),
    }
}

#[derive(Deserialize)]
struct PickChoose {
    project: String,
    agent: Option<String>,
    adapter: Option<String>,
    role: Option<String>,
}

/// `POST /api/v1/pick/{code}`: the person chooses the project on the dashboard.
async fn pick_choose(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Json(b): Json<PickChoose>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !crate::protocol::is_slug(&b.project) {
        return err(
            StatusCode::BAD_REQUEST,
            "names use letters, digits, dots, dashes and underscores",
        );
    }
    let now = crate::now_ms();
    let mine = gw
        .picks
        .locked()
        .get(&code)
        .is_some_and(|p| p.tenant == a.id && now - p.at < PICK_TTL_MS);
    if !mine {
        return err(
            StatusCode::NOT_FOUND,
            "that choice expired; run the command again",
        );
    }
    // Nothing continues without a Discord bot that is really in a server with the permissions it needs. (A saved bot is one Discord confirmed the
    // token of; being in a server with the right permissions is checked with Discord now.)
    if let Err(why) = super::bots::account_ready(&gw, &a.id).await {
        return err(StatusCode::CONFLICT, &why);
    }
    if let Some(p) = gw.picks.locked().get_mut(&code) {
        p.chosen = Some(b.project);
        p.agent = b
            .agent
            .filter(|n| crate::protocol::agent_name_problem(n).is_none() && !n.is_empty());
        p.adapter = b
            .adapter
            .filter(|a| matches!(a.as_str(), "claude" | "codex" | "agy"));
        p.role = b
            .role
            .map(|r| {
                r.chars()
                    .filter(|c| !c.is_control())
                    .take(100)
                    .collect::<String>()
            })
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
    }
    json_reply(StatusCode::OK, json!({ "ok": true }))
}

/// Whether the gateway can do its job: its database answers.
async fn readyz(State(gw): State<Gateway>) -> Response {
    let db = gw.control.account("t0000000000000000").is_ok();
    json_reply(
        if db {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        json!({"ok": db, "checks": {"database": db}, "tenants": gw.registry.running()}),
    )
}

// ----- signing in -----

/// A page to go to after signing in, only if it is a path on this site (never another address).
fn safe_next(n: &str) -> Option<String> {
    (n.starts_with('/')
        && !n.starts_with("//")
        && !n.contains('\\')
        && n.len() <= 200
        && n.chars().all(|c| !c.is_control()))
    .then(|| n.to_string())
}

/// Sends the browser to Discord to sign in.
async fn auth_start(
    State(gw): State<Gateway>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // Local preview: no Discord, so the sign-in page is the made-up person.
    if gw.dev && gw.oauth.is_none() {
        let next = q
            .get("next")
            .and_then(|n| safe_next(n))
            .unwrap_or_else(|| "/".into());
        return Redirect::to(&format!("/auth/dev?next={}", pct(&next))).into_response();
    }
    let Some(o) = gw.oauth.clone() else {
        return page(
            StatusCode::NOT_FOUND,
            "sign-in with Discord is not set up here",
        );
    };
    let state = random_hex();
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope=identify&state={state}&prompt=none",
        o.authorize_url,
        pct(&o.client_id),
        pct(&o.redirect_uri)
    );
    let mut r = Redirect::to(&url).into_response();
    set_cookie(&mut r, "cc_state", &state, "/auth", 600, &o);
    if let Some(next) = q.get("next").and_then(|n| safe_next(n)) {
        set_cookie(&mut r, "cc_next", &pct(&next), "/auth", 600, &o);
    }
    secure(r, "text/plain", "no-store")
}

/// Where Discord sends the person back to: the code is swapped for their account id, and a session starts.
async fn auth_callback(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(o) = gw.oauth.clone() else {
        return page(
            StatusCode::NOT_FOUND,
            "sign-in with Discord is not set up here",
        );
    };
    let (ip, now) = (ip_of(peer), crate::now_ms());
    if gw.failures.locked().blocked(&ip, now as f64) {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts, wait a minute",
        );
    }
    let refuse = |status, text: &str| {
        crate::warn!("gateway", "sign-in from {ip} refused: {text}");
        gw.failures.locked().fail(&ip, now as f64);
        page(status, text)
    };
    let (Some(code), Some(state), Some(mine)) =
        (q.get("code"), q.get("state"), cookie(&headers, "cc_state"))
    else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the sign-in expired or did not start here, try again",
        );
    };
    if !same(state, &mine) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the sign-in did not start here, try again",
        );
    }
    let Ok((discord_id, name)) = identify(&o, code).await else {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "Discord did not confirm the sign-in",
        );
    };
    let sid = gw
        .control
        .sign_in_discord(&discord_id, &name, now)
        .and_then(|a| gw.control.create_session(&a.id, now));
    let Ok(sid) = sid else {
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the sign-in",
        );
    };
    crate::info!("gateway", "{name} (Discord {discord_id}) signed in");
    let next = cookie(&headers, "cc_next")
        .and_then(|n| percent_decode(&n))
        .and_then(|n| safe_next(&n))
        .unwrap_or_else(|| "/".into());
    let mut r = Redirect::to(&next).into_response();
    set_cookie(
        &mut r,
        "cc_session",
        &sid,
        "/",
        crate::control::SESSION_MS / 1000,
        &o,
    );
    set_cookie(&mut r, "cc_state", "", "/auth", 0, &o);
    set_cookie(&mut r, "cc_next", "", "/auth", 0, &o);
    secure(r, "text/plain", "no-store")
}

/// Local preview only: signs in a made-up person without Discord. Not registered as a route's behaviour unless `dev` is on.
async fn auth_dev(State(gw): State<Gateway>, Query(q): Query<HashMap<String, String>>) -> Response {
    if !gw.dev {
        return page(StatusCode::NOT_FOUND, "not found");
    }
    let name: String = q
        .get("name")
        .map_or("Dev", String::as_str)
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .take(30)
        .collect();
    let now = crate::now_ms();
    let sid = gw
        .control
        .sign_in_discord("900001", &name, now)
        .and_then(|a| gw.control.create_session(&a.id, now));
    let Ok(sid) = sid else {
        return page(StatusCode::INTERNAL_SERVER_ERROR, "could not sign in");
    };
    let next = q
        .get("next")
        .and_then(|n| safe_next(n))
        .unwrap_or_else(|| "/".into());
    let mut r = Redirect::to(&next).into_response();
    let o = Oauth {
        client_id: String::new(),
        client_secret: String::new(),
        redirect_uri: gw.public_url.clone(),
        authorize_url: String::new(),
        api_base: String::new(),
    };
    set_cookie(
        &mut r,
        "cc_session",
        &sid,
        "/",
        crate::control::SESSION_MS / 1000,
        &o,
    );
    secure(r, "text/plain", "no-store")
}

/// Undoes `pct`.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Signs out: the session is ended here, not just forgotten by the browser.
async fn auth_logout(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    if let Some(sid) = cookie(&headers, "cc_session") {
        let _ = gw.control.end_session(&sid);
    }
    let mut r = page(StatusCode::NO_CONTENT, "");
    if let Some(o) = &gw.oauth {
        set_cookie(&mut r, "cc_session", "", "/", 0, o);
    }
    r
}

/// Who is signed in, for the page to show.
async fn me(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    match account_of(&gw, &headers) {
        Some(a) => json_reply(StatusCode::OK, json!({"id": a.id, "name": a.name})),
        None => err(StatusCode::UNAUTHORIZED, "not signed in"),
    }
}

// ----- a machine asks to be approved -----

#[derive(Deserialize, Default)]
struct CodeAsk {
    #[serde(default)]
    node: String,
}

/// A machine with no token asks for a code. Anyone may ask (the machine has no account yet), so asking is limited per address.
async fn device_code(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Option<Json<CodeAsk>>,
) -> Response {
    let (ip, now) = (ip_of(peer), crate::now_ms());
    {
        let mut l = gw.code_asks.locked();
        if l.blocked(&ip, now as f64) {
            return err(
                StatusCode::TOO_MANY_REQUESTS,
                "too many requests, wait a minute",
            );
        }
        l.fail(&ip, now as f64);
    }
    let hint = body.map(|b| b.0.node).unwrap_or_default();
    let hint = if valid_node(&hint) {
        hint
    } else {
        String::new()
    };
    match gw.control.device_start(&hint, now) {
        Ok((device, user)) => json_reply(
            StatusCode::OK,
            json!({
                "device_code": device,
                "user_code": user,
                "verify_url": format!("{}/activate?code={user}", gw.public_url),
                "interval": 3,
                "expires_in": crate::control::DEVICE_CODE_MS / 1000,
            }),
        ),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "could not make a code"),
    }
}

#[derive(Deserialize)]
struct PollAsk {
    device_code: String,
}

/// The machine asks whether a person has approved it yet.
async fn device_token(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(b): Json<PollAsk>,
) -> Response {
    let (ip, now) = (ip_of(peer), crate::now_ms());
    if gw.failures.locked().blocked(&ip, now as f64) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts, wait a minute",
        );
    }
    match gw.control.device_poll(&b.device_code, now) {
        Ok(Poll::Pending) => json_reply(StatusCode::ACCEPTED, json!({"status": "pending"})),
        Ok(Poll::Approved {
            token,
            tenant,
            node,
        }) => json_reply(
            StatusCode::OK,
            json!({"token": token, "tenant": tenant, "node": node}),
        ),
        Ok(Poll::Gone) => {
            gw.failures.locked().fail(&ip, now as f64);
            err(
                StatusCode::GONE,
                "this code is no longer valid, start again",
            )
        }
        Err(_) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not check the code",
        ),
    }
}

/// Guesses at user codes are failures too, so the 8-letter code cannot be brute-forced from one address.
fn guess_blocked(gw: &Gateway, ip: &str) -> bool {
    gw.failures.locked().blocked(ip, crate::now_ms() as f64)
}

/// The approval page asks what machine a typed code is for.
async fn device_lookup(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if account_of(&gw, &headers).is_none() {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    }
    let ip = ip_of(peer);
    if guess_blocked(&gw, &ip) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many wrong codes, wait a minute",
        );
    }
    let code = q.get("code").map_or("", String::as_str);
    match gw.control.device_lookup(code, crate::now_ms()) {
        Ok(Some(p)) => json_reply(
            StatusCode::OK,
            json!({"code": p.user_code, "node": p.node_hint}),
        ),
        _ => {
            gw.failures.locked().fail(&ip, crate::now_ms() as f64);
            err(StatusCode::NOT_FOUND, "that code is wrong or has expired")
        }
    }
}

#[derive(Deserialize)]
struct ApproveAsk {
    code: String,
    node: String,
}

/// A signed-in person approves a machine under a name. Needs the session cookie and a JSON body, so another website cannot make the
/// browser do it (the cookie is SameSite=Lax, and a cross-site form cannot send JSON).
async fn device_approve(
    State(gw): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(b): Json<ApproveAsk>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let ip = ip_of(peer);
    if guess_blocked(&gw, &ip) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many wrong codes, wait a minute",
        );
    }
    if !valid_node(&b.node) {
        return err(
            StatusCode::BAD_REQUEST,
            "a machine name is letters, digits, dot, dash and underscore, up to 40",
        );
    }
    match gw
        .control
        .device_approve(&b.code, &account.id, &b.node, crate::now_ms())
    {
        Ok(true) => {
            crate::info!(
                "gateway",
                "account {} approved machine {}",
                account.id,
                b.node
            );
            json_reply(StatusCode::OK, json!({"ok": true}))
        }
        _ => {
            gw.failures.locked().fail(&ip, crate::now_ms() as f64);
            err(
                StatusCode::NOT_FOUND,
                "that code is wrong, expired, already used, or this account has too many machines",
            )
        }
    }
}

#[derive(Deserialize)]
struct DenyAsk {
    code: String,
}

/// A signed-in person refuses a machine.
async fn device_deny(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Json(b): Json<DenyAsk>,
) -> Response {
    if account_of(&gw, &headers).is_none() {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    }
    let _ = gw.control.device_deny(&b.code);
    json_reply(StatusCode::OK, json!({"ok": true}))
}

// ----- the account's own data -----

/// The account's machines, agents, asks and tasks, from its own hub.
async fn state(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub could not start");
    };
    // Everything in an account's own hub belongs to its owner.
    let body = state_json(&hub.state, Who::Everything).await;
    json_reply(StatusCode::OK, body)
}

/// The machines the account has approved.
async fn machines(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    match gw.control.machines(&account.id) {
        Ok(m) => json_reply(
            StatusCode::OK,
            json!(
                m.iter()
                    .map(|(n, at)| json!({"node": n, "since": at}))
                    .collect::<Vec<_>>()
            ),
        ),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "could not list machines"),
    }
}

#[derive(Deserialize)]
struct RevokeAsk {
    node: String,
}

/// Cancels a machine's tokens: it can no longer connect.
async fn machine_revoke(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Json(b): Json<RevokeAsk>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let n = gw.control.revoke_machine(&account.id, &b.node).unwrap_or(0);
    json_reply(StatusCode::OK, json!({"revoked": n}))
}

/// The graphs and numbers of the Insights page: who talks to whom, each agent's states over time, task and ask flow, turns and estimated
/// tokens, how long agents take to pick messages up, and how healthy the hub and each bot's bridge have been. `?range=1h|24h|7d|30d`.
async fn insights(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub could not start");
    };
    const MIN: i64 = 60_000;
    let (range, bucket) = match q.get("range").map_or("24h", String::as_str) {
        "1h" => (60 * MIN, MIN),
        "7d" => (7 * 24 * 60 * MIN, 2 * 60 * MIN),
        "30d" => (30 * 24 * 60 * MIN, 8 * 60 * MIN),
        _ => (24 * 60 * MIN, 15 * MIN),
    };
    let now = crate::now_ms();
    let reader = hub.state.reader.clone();
    let since = now - range;
    let events = tokio::task::spawn_blocking(move || {
        let g = reader.locked();
        g.as_ref()
            .and_then(|s| s.events_since(None, since).ok())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let summary = crate::metrics::events::summarize(&events, since, now, bucket);
    // How long an agent took to pick up a message is kept by the core for the last day.
    let minutes = (range / MIN).min(1440) as usize;
    let pickup = hub
        .state
        .handle
        .read(move |c, now| c.metrics.insights(minutes, minutes.max(1), &[], now))
        .await
        .map(|i| json!({"n": i.acceptance.n, "p50_ms": i.acceptance.p50_ms, "p95_ms": i.acceptance.p95_ms}));
    let health = super::health::summary(&hub.state).await;
    json_reply(
        StatusCode::OK,
        json!({"range_ms": range, "summary": summary, "pickup": pickup, "health": health}),
    )
}

/// The account's whole conversation as an Obsidian vault in a `.tar.gz` (the same notes `claudecord export` writes). Nobody else's history
/// is in it: it reads this account's own database.
// ponytail: the whole history is read into memory, so it refuses past 200,000 rows; stream per project when someone has more.
async fn export(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub could not start");
    };
    let reader = hub.state.reader.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let g = reader.locked();
        let s = g.as_ref().ok_or("no database")?;
        let mut rows = Vec::new();
        for p in s.projects().map_err(|e| e.to_string())? {
            rows.extend(s.history_all(&p).map_err(|e| e.to_string())?);
            if rows.len() > 200_000 {
                return Err("too much history to export in one go".into());
            }
        }
        crate::export::tar_gz(&crate::export::obsidian::render(&rows)).map_err(|e| e.to_string())
    })
    .await;
    match built {
        Ok(Ok(bytes)) => {
            let mut r = bytes.into_response();
            let h = r.headers_mut();
            h.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/gzip"),
            );
            h.insert(
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static(
                    "attachment; filename=\"claudecord-vault.tar.gz\"",
                ),
            );
            h.insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            r
        }
        Ok(Err(e)) => err(StatusCode::PAYLOAD_TOO_LARGE, &e),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "the export failed"),
    }
}
