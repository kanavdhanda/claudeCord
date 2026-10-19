//! Terminal streaming control plane, short-lived grant issuance, WebRTC signaling, and E2E-encrypted WebSocket fallback relay.
//!
//! Responsibilities:
//! - Derives per-node HMAC-SHA256 grant verification keys from a hub-local secret and delivers each node's key in `HubFrame::Welcome`.
//! - Authenticates dashboard users and project owners/members before issuing short-lived `TerminalGrantClaims` tokens.
//! - Routes WebRTC SDP offers/answers and trickle ICE candidates between the browser and the target host node.
//! - Relays E2E-encrypted (`e2e1.`) `TermFrame` envelopes over `/api/v1/terminal/ws` when direct/TURN WebRTC is unavailable or falls back.
//! - Enforces grant expiry, immediate revocation, slow-reader bounds, and audit logging without ever logging terminal input/output payloads.

use super::gateway::{Gateway, account_of};
use super::login::{random_hex, same};
use super::web::{Who, authorised, denied, secure};
use super::{AppState, now_ms};
use crate::hub::{Effect, Persist};
use crate::protocol::terminal::{
    TERMINAL_GRANT_TTL_SECS, TerminalGrantClaims, derive_node_grant_key, sign_terminal_grant,
};
use crate::protocol::{HubFrame, IceServerSpec, NodeFrame};
use crate::sync::Lock;
use axum::{
    Json,
    extract::{
        ConnectInfo, Path, Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// Maximum buffered frames per browser terminal WebSocket before a slow reader lags.
const BROWSER_WS_CAPACITY: usize = 256;

/// Maximum time a single WebSocket send to a browser client may stall before closing.
const BROWSER_SEND_STALL: Duration = Duration::from_secs(10);

/// Default terminal grant lifetime in milliseconds (`TERMINAL_GRANT_TTL_SECS * 1000`).
const DEFAULT_GRANT_TTL_MS: i64 = TERMINAL_GRANT_TTL_SECS * 1000;

/// Maximum terminal grant lifetime in milliseconds (10 minutes, matching `TerminalGrantClaims::is_valid`).
const MAX_GRANT_TTL_MS: i64 = 600_000;

/// Minimum terminal grant lifetime in milliseconds (1 second).
const MIN_GRANT_TTL_MS: i64 = 1_000;

/// Observability counters for terminal streaming sessions and transport paths.
#[derive(Default)]
pub struct TermMetrics {
    pub sessions_created: AtomicU64,
    pub sessions_revoked: AtomicU64,
    pub webrtc_connected: AtomicU64,
    pub wss_fallback: AtomicU64,
    pub frames_relayed: AtomicU64,
    pub bytes_relayed: AtomicU64,
    pub auth_failures: AtomicU64,
    pub slow_reader_drops: AtomicU64,
}

/// How many frames from the device are kept for a browser that has not connected yet (the first tabs and screen are two).
const EARLY_FRAMES_MAX: usize = 8;

/// Active terminal streaming session tracked by the hub broker.
pub struct TermSession {
    pub session_id: String,
    pub project: String,
    pub agent_id: String,
    pub node: String,
    pub user_id: String,
    pub jti: String,
    pub generation: u64,
    pub grant: String,
    pub e2e_key: String,
    pub read: bool,
    pub write: bool,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub revoked: bool,
    pub last_ready: Option<NodeFrame>,
    pub last_state: Option<NodeFrame>,
    /// Frames the device sent before any browser was listening (its first tabs and screen). Handed to the first browser that connects, since the
    /// broadcast channel keeps nothing for a listener that is not there yet.
    pub early: Vec<NodeFrame>,
    /// Set once a browser has connected to the relay; from then on frames go through the broadcast channel only.
    pub attached: bool,
    pub browser_tx: broadcast::Sender<NodeFrame>,
}

/// Per-hub broker managing short-lived terminal grants, signaling, and WSS fallback relay.
pub struct TerminalBroker {
    grant_secret: [u8; 32],
    sessions: Mutex<HashMap<String, TermSession>>,
    ice_servers: Vec<IceServerSpec>,
    next_generation: AtomicU64,
    pub metrics: TermMetrics,
}

impl Default for TerminalBroker {
    /// Creates a broker with a fresh CSPRNG grant secret and optional ICE servers from `CLAUDECORD_ICE_SERVERS`.
    fn default() -> Self {
        Self::new()
    }
}

/// Generates 32 random bytes using `random_hex` (which yields 64 hex characters = 32 bytes).
fn random_32_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    let hex = random_hex();
    for (i, slot) in out.iter_mut().enumerate() {
        if let Some(pair) = hex.get(i * 2..i * 2 + 2)
            && let Ok(b) = u8::from_str_radix(pair, 16)
        {
            *slot = b;
        }
    }
    out
}

impl TerminalBroker {
    /// Creates a new `TerminalBroker` with a random 32-byte master grant secret.
    pub fn new() -> Self {
        let grant_secret = random_32_bytes();
        let ice_servers = std::env::var("CLAUDECORD_ICE_SERVERS")
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<IceServerSpec>>(&raw).ok())
            .unwrap_or_default();
        Self {
            grant_secret,
            sessions: Mutex::new(HashMap::new()),
            ice_servers,
            next_generation: AtomicU64::new(1),
            metrics: TermMetrics::default(),
        }
    }

    /// Derives the 32-byte HMAC-SHA256 grant verification key for a specific host node.
    pub fn node_grant_key(&self, node: &str) -> [u8; 32] {
        derive_node_grant_key(&self.grant_secret, node)
    }

    /// Returns the base64url-encoded 32-byte grant verification key sent to `node` in `HubFrame::Welcome`.
    pub fn node_grant_key_b64(&self, node: &str) -> String {
        B64.encode(self.node_grant_key(node))
    }

    /// Routes `NodeFrame::{TermReady, TermFrame, TermState}` from an authenticated host node to subscribed browsers.
    pub fn on_node_frame(&self, node: &str, frame: &NodeFrame) {
        let mut sessions = self.sessions.locked();
        match frame {
            NodeFrame::TermReady {
                session_id,
                transport,
                ..
            } => {
                let Some(s) = sessions.get_mut(session_id) else {
                    return;
                };
                if s.node != node || s.revoked {
                    return;
                }
                if transport.starts_with("webrtc") {
                    self.metrics
                        .webrtc_connected
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    self.metrics.wss_fallback.fetch_add(1, Ordering::Relaxed);
                }
                s.last_ready = Some(frame.clone());
                let _ = s.browser_tx.send(frame.clone());
            }
            NodeFrame::TermState {
                session_id,
                transport,
                ..
            } => {
                let Some(s) = sessions.get_mut(session_id) else {
                    return;
                };
                if s.node != node {
                    return;
                }
                if transport == "wss_fallback" {
                    self.metrics.wss_fallback.fetch_add(1, Ordering::Relaxed);
                }
                s.last_state = Some(frame.clone());
                let _ = s.browser_tx.send(frame.clone());
            }
            NodeFrame::TermFrame {
                session_id,
                payload,
                ..
            } => {
                let Some(s) = sessions.get(session_id) else {
                    return;
                };
                if s.node != node || s.revoked {
                    return;
                }
                self.metrics.frames_relayed.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .bytes_relayed
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                let s = sessions.get_mut(session_id).expect("looked up above");
                if !s.attached && s.early.len() < EARLY_FRAMES_MAX {
                    s.early.push(frame.clone());
                }
                let _ = s.browser_tx.send(frame.clone());
            }
            _ => {}
        }
    }

    /// Snapshot of terminal streaming metrics as JSON.
    pub fn metrics_json(&self, now: i64) -> Value {
        let sessions = self.sessions.locked();
        let active = sessions
            .values()
            .filter(|s| !s.revoked && now <= s.expires_at_ms)
            .count();
        json!({
            "sessionsCreated": self.metrics.sessions_created.load(Ordering::Relaxed),
            "sessionsActive": active,
            "sessionsRevoked": self.metrics.sessions_revoked.load(Ordering::Relaxed),
            "webrtcConnected": self.metrics.webrtc_connected.load(Ordering::Relaxed),
            "wssFallback": self.metrics.wss_fallback.load(Ordering::Relaxed),
            "framesRelayed": self.metrics.frames_relayed.load(Ordering::Relaxed),
            "bytesRelayed": self.metrics.bytes_relayed.load(Ordering::Relaxed),
            "authFailures": self.metrics.auth_failures.load(Ordering::Relaxed),
            "slowReaderDrops": self.metrics.slow_reader_drops.load(Ordering::Relaxed),
        })
    }
}

/// Request body for `POST /api/v1/terminal/sessions`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionReq {
    pub project: String,
    pub agent: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub offer_sdp: Option<String>,
    #[serde(default)]
    pub read: Option<bool>,
    #[serde(default)]
    pub write: Option<bool>,
    #[serde(default)]
    pub ttl_ms: Option<i64>,
    #[serde(default)]
    pub e2e: Option<bool>,
}

/// Request body for `POST /api/v1/terminal/sessions/{id}/signal`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SignalSessionReq {
    #[serde(default)]
    pub offer_sdp: Option<String>,
    #[serde(default)]
    pub candidate: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub reconnect: Option<bool>,
}

/// Request body for `POST /api/v1/terminal/sessions/{id}/revoke`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RevokeSessionReq {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Incoming JSON message from the browser over `/api/v1/terminal/ws`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserWsIn {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    candidate: Option<String>,
    #[serde(default)]
    offer_sdp: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    reason: Option<String>,
}

/// Formats a JSON HTTP response with security headers.
fn reply_json(status: StatusCode, body: Value) -> Response {
    secure(
        (status, Json(body)).into_response(),
        "application/json",
        "no-store",
    )
}

/// Creates a short-lived terminal session grant and initiates the stream with the target host node.
pub(super) async fn create_session_for(st: &AppState, who: Who, req: CreateSessionReq) -> Response {
    let project = req.project.trim().to_string();
    let raw_agent = req.agent.trim().to_string();
    if project.is_empty() || raw_agent.is_empty() {
        return reply_json(
            StatusCode::BAD_REQUEST,
            json!({"error": "project and agent are required"}),
        );
    }
    let agent_id = if raw_agent.contains('/') {
        raw_agent
    } else {
        format!("{project}/{raw_agent}")
    };
    let cols = req.cols.unwrap_or(120).clamp(20, 500);
    let rows = req.rows.unwrap_or(36).clamp(5, 250);
    let transport = req
        .transport
        .unwrap_or_else(|| "auto".into())
        .trim()
        .to_string();
    let read = req.read.unwrap_or(true);
    let write = req.write.unwrap_or(true);
    let ttl_ms = req
        .ttl_ms
        .unwrap_or(DEFAULT_GRANT_TTL_MS)
        .clamp(MIN_GRANT_TTL_MS, MAX_GRANT_TTL_MS);
    let now = now_ms();
    let now_s = now / 1000;
    let ttl_s = (ttl_ms / 1000).clamp(1, 600);
    let expires_at_ms = (now_s + ttl_s) * 1000;
    let session_id = format!("ts-{}", &random_hex()[..24]);
    let jti = format!("jti-{}", &random_hex()[..24]);
    let e2e_key = if req.e2e.unwrap_or(true) {
        B64.encode(random_32_bytes())
    } else {
        String::new()
    };
    let user_id = who.user_id();

    enum Lookup {
        Denied,
        NotFound,
        Offline(String),
        /// The machine runs a claudecord from before terminals existed: it would ignore the request and the page would stay blank.
        TooOld {
            node: String,
            version: String,
        },
        Ready {
            node: String,
        },
    }

    let project_chk = project.clone();
    let agent_chk = agent_id.clone();
    let who_chk = who.clone();
    let lookup = st
        .handle
        .read(move |c, _| {
            if !who_chk.may_see(c, &project_chk) {
                return Lookup::Denied;
            }
            let Some(a) = c.agent(&agent_chk) else {
                return Lookup::NotFound;
            };
            if a.project != project_chk {
                return Lookup::NotFound;
            }
            match c.conn_of(&a.node_name) {
                Some(_) => match c.node_too_old_for_terminal(&a.node_name) {
                    Some(version) => Lookup::TooOld {
                        node: a.node_name.clone(),
                        version,
                    },
                    None => Lookup::Ready {
                        node: a.node_name.clone(),
                    },
                },
                None => Lookup::Offline(a.node_name.clone()),
            }
        })
        .await;

    let node = match lookup {
        Some(Lookup::Ready { node }) => node,
        Some(Lookup::Denied) => {
            st.terminals
                .metrics
                .auth_failures
                .fetch_add(1, Ordering::Relaxed);
            return reply_json(
                StatusCode::FORBIDDEN,
                json!({"error": "no access to this project"}),
            );
        }
        Some(Lookup::NotFound) => {
            return reply_json(
                StatusCode::NOT_FOUND,
                json!({"error": "no such agent in this project"}),
            );
        }
        Some(Lookup::TooOld { node, version }) => {
            return reply_json(
                StatusCode::CONFLICT,
                json!({"error": format!(
                    "machine {node} runs claudecord {version}, which cannot show a terminal. Update it (npm i -g claudecord@latest), then run `claudecord down` on that machine and start the agent again"
                )}),
            );
        }
        Some(Lookup::Offline(n)) => {
            return reply_json(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": format!("machine {n} is offline")}),
            );
        }
        None => {
            return reply_json(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": "hub unavailable"}),
            );
        }
    };

    let generation = st.terminals.next_generation.fetch_add(1, Ordering::Relaxed);
    let claims = TerminalGrantClaims {
        jti: jti.clone(),
        sub: user_id.clone(),
        aud: node.clone(),
        prj: project.clone(),
        agt: agent_id.clone(),
        generation,
        read,
        write,
        iat: now_s,
        exp: now_s + ttl_s,
        dtls_fp: None,
    };
    let node_key = st.terminals.node_grant_key(&node);
    let Some(grant) = sign_terminal_grant(&claims, &node_key) else {
        return reply_json(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid terminal grant claims"}),
        );
    };
    let ice_servers = st.terminals.ice_servers.clone();

    let (browser_tx, _) = broadcast::channel(BROWSER_WS_CAPACITY);
    {
        let mut sessions = st.terminals.sessions.locked();
        sessions.retain(|_, s| now - s.created_at_ms < 3_600_000);
        sessions.insert(
            session_id.clone(),
            TermSession {
                session_id: session_id.clone(),
                project: project.clone(),
                agent_id: agent_id.clone(),
                node: node.clone(),
                user_id: user_id.clone(),
                jti: jti.clone(),
                generation,
                grant: grant.clone(),
                e2e_key: e2e_key.clone(),
                read,
                write,
                created_at_ms: now,
                expires_at_ms,
                revoked: false,
                last_ready: None,
                last_state: None,
                early: Vec::new(),
                attached: false,
                browser_tx,
            },
        );
    }
    st.terminals
        .metrics
        .sessions_created
        .fetch_add(1, Ordering::Relaxed);

    let open_frame = HubFrame::TermOpen {
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        grant: grant.clone(),
        generation,
        transport: transport.clone(),
        offer_sdp: req.offer_sdp,
        e2e_key: (!e2e_key.is_empty()).then(|| e2e_key.clone()),
        ice_servers: ice_servers.clone(),
        cols,
        rows,
    };
    let audit_project = project.clone();
    let audit_user = user_id.clone();
    let audit_what = format!(
        "issued terminal grant {jti} for session {session_id} on {agent_id} (write={write})"
    );
    let target_node = node.clone();
    let _ = st
        .handle
        .call(move |c, at| {
            let mut fx = vec![Effect::Persist(Persist::Audit {
                project: audit_project,
                who: audit_user,
                what: audit_what,
                at,
            })];
            if let Some(conn) = c.conn_of(&target_node) {
                fx.push(Effect::Send {
                    conn,
                    frame: open_frame,
                });
            }
            ((), fx)
        })
        .await;

    reply_json(
        StatusCode::OK,
        json!({
            "sessionId": session_id,
            "project": project,
            "agentId": agent_id,
            "node": node,
            "jti": jti,
            "grant": grant,
            "generation": generation,
            "read": read,
            "write": write,
            "expiresAtMs": expires_at_ms,
            "e2eKey": e2e_key,
            "iceServers": ice_servers,
            "wsPath": "/api/v1/terminal/ws",
        }),
    )
}

/// Signals an existing terminal session (SDP offer, reconnect generation bump, or trickle ICE candidate).
pub(super) async fn signal_session_for(
    st: &AppState,
    who: Who,
    session_id: String,
    req: SignalSessionReq,
) -> Response {
    let now = now_ms();
    let now_s = now / 1000;
    let meta = {
        let sessions = st.terminals.sessions.locked();
        sessions.get(&session_id).map(|s| {
            (
                s.project.clone(),
                s.agent_id.clone(),
                s.node.clone(),
                s.revoked,
                s.expires_at_ms,
            )
        })
    };
    let Some((project, agent_id, node, revoked, expires_at_ms)) = meta else {
        return reply_json(
            StatusCode::NOT_FOUND,
            json!({"error": "no such terminal session"}),
        );
    };
    if revoked || now > expires_at_ms {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return reply_json(
            StatusCode::GONE,
            json!({"error": "terminal session grant expired or revoked"}),
        );
    }
    let project_chk = project.clone();
    let who_chk = who.clone();
    let allowed = st
        .handle
        .read(move |c, _| who_chk.may_see(c, &project_chk))
        .await
        .unwrap_or(false);
    if !allowed {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return reply_json(
            StatusCode::FORBIDDEN,
            json!({"error": "no access to this project"}),
        );
    }

    let mut frames_to_send = Vec::new();
    let (generation, grant) = {
        let mut sessions = st.terminals.sessions.locked();
        let Some(s) = sessions.get_mut(&session_id) else {
            return reply_json(
                StatusCode::NOT_FOUND,
                json!({"error": "no such terminal session"}),
            );
        };
        let needs_open =
            req.offer_sdp.is_some() || req.reconnect.unwrap_or(false) || req.transport.is_some();
        if needs_open {
            s.generation = st
                .terminals
                .next_generation
                .fetch_add(1, Ordering::Relaxed)
                .max(s.generation + 1);
            s.jti = format!("jti-{}", &random_hex()[..24]);
            s.expires_at_ms = (now_s + TERMINAL_GRANT_TTL_SECS) * 1000;
            let claims = TerminalGrantClaims {
                jti: s.jti.clone(),
                sub: s.user_id.clone(),
                aud: s.node.clone(),
                prj: s.project.clone(),
                agt: s.agent_id.clone(),
                generation: s.generation,
                read: s.read,
                write: s.write,
                iat: now_s,
                exp: now_s + TERMINAL_GRANT_TTL_SECS,
                dtls_fp: None,
            };
            let node_key = st.terminals.node_grant_key(&s.node);
            if let Some(new_grant) = sign_terminal_grant(&claims, &node_key) {
                s.grant = new_grant;
            }
            frames_to_send.push(HubFrame::TermOpen {
                session_id: s.session_id.clone(),
                agent_id: agent_id.clone(),
                grant: s.grant.clone(),
                generation: s.generation,
                transport: req.transport.unwrap_or_else(|| "auto".into()),
                offer_sdp: req.offer_sdp,
                e2e_key: (!s.e2e_key.is_empty()).then(|| s.e2e_key.clone()),
                ice_servers: st.terminals.ice_servers.clone(),
                cols: req.cols.unwrap_or(120).clamp(20, 500),
                rows: req.rows.unwrap_or(36).clamp(5, 250),
            });
        }
        if let Some(cand) = req.candidate.filter(|c| !c.trim().is_empty()) {
            frames_to_send.push(HubFrame::TermIce {
                session_id: s.session_id.clone(),
                candidate: cand,
            });
        }
        (s.generation, s.grant.clone())
    };

    if !frames_to_send.is_empty() {
        let target_node = node.clone();
        let _ = st
            .handle
            .call(move |c, _| {
                let mut fx = Vec::new();
                if let Some(conn) = c.conn_of(&target_node) {
                    for frame in frames_to_send {
                        fx.push(Effect::Send { conn, frame });
                    }
                }
                ((), fx)
            })
            .await;
    }

    reply_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "sessionId": session_id,
            "generation": generation,
            "grant": grant,
        }),
    )
}

/// Explicitly revokes a terminal session and tells the host node to terminate the stream worker immediately.
pub(super) async fn revoke_session_for(
    st: &AppState,
    who: Who,
    session_id: String,
    req: RevokeSessionReq,
) -> Response {
    let meta = {
        let sessions = st.terminals.sessions.locked();
        sessions.get(&session_id).map(|s| {
            (
                s.project.clone(),
                s.agent_id.clone(),
                s.node.clone(),
                s.jti.clone(),
            )
        })
    };
    let Some((project, agent_id, node, jti)) = meta else {
        return reply_json(
            StatusCode::NOT_FOUND,
            json!({"error": "no such terminal session"}),
        );
    };
    let project_chk = project.clone();
    let who_chk = who.clone();
    let allowed = st
        .handle
        .read(move |c, _| who_chk.may_see(c, &project_chk))
        .await
        .unwrap_or(false);
    if !allowed {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return reply_json(
            StatusCode::FORBIDDEN,
            json!({"error": "no access to this project"}),
        );
    }

    let reason = req
        .reason
        .unwrap_or_else(|| "revoked by user".into())
        .chars()
        .take(200)
        .collect::<String>();
    {
        let mut sessions = st.terminals.sessions.locked();
        if let Some(s) = sessions.get_mut(&session_id) {
            s.revoked = true;
            let state_frame = NodeFrame::TermState {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                state: "revoked".into(),
                transport: "none".into(),
                rtt_ms: None,
                bytes_tx: 0,
                bytes_rx: 0,
                detail: Some(reason.clone()),
            };
            s.last_state = Some(state_frame.clone());
            let _ = s.browser_tx.send(state_frame);
        }
    }
    st.terminals
        .metrics
        .sessions_revoked
        .fetch_add(1, Ordering::Relaxed);

    let close_frame = HubFrame::TermClose {
        session_id: session_id.clone(),
        jti: Some(jti.clone()),
        reason: reason.clone(),
    };
    let audit_user = who.user_id();
    let audit_what =
        format!("revoked terminal grant {jti} for session {session_id} on {agent_id}: {reason}");
    let _ = st
        .handle
        .call(move |c, at| {
            let mut fx = vec![Effect::Persist(Persist::Audit {
                project,
                who: audit_user,
                what: audit_what,
                at,
            })];
            if let Some(conn) = c.conn_of(&node) {
                fx.push(Effect::Send {
                    conn,
                    frame: close_frame,
                });
            }
            ((), fx)
        })
        .await;

    reply_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "sessionId": session_id,
            "revoked": true,
        }),
    )
}

/// Upgrades a browser connection to `/api/v1/terminal/ws` after verifying session authorization and grant validity.
pub(super) async fn ws_for(
    st: AppState,
    who_opt: Option<Who>,
    query: HashMap<String, String>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(session_id) = query.get("session_id").cloned() else {
        return reply_json(
            StatusCode::BAD_REQUEST,
            json!({"error": "session_id query parameter is required"}),
        );
    };
    let now = now_ms();
    let meta = {
        let sessions = st.terminals.sessions.locked();
        sessions.get(&session_id).map(|s| {
            (
                s.project.clone(),
                s.grant.clone(),
                s.revoked,
                s.expires_at_ms,
            )
        })
    };
    let Some((project, expected_grant, revoked, expires_at_ms)) = meta else {
        return reply_json(
            StatusCode::NOT_FOUND,
            json!({"error": "no such terminal session"}),
        );
    };
    if revoked || now > expires_at_ms {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return reply_json(
            StatusCode::GONE,
            json!({"error": "terminal session grant expired or revoked"}),
        );
    }

    let grant_matches = query
        .get("grant")
        .is_some_and(|g| same(g.as_str(), expected_grant.as_str()));
    let authorized = if grant_matches {
        true
    } else if let Some(who) = who_opt {
        let project_chk = project.clone();
        st.handle
            .read(move |c, _| who.may_see(c, &project_chk))
            .await
            .unwrap_or(false)
    } else {
        false
    };
    if !authorized {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return denied();
    }

    ws.max_message_size(st.cfg.max_frame)
        .max_frame_size(st.cfg.max_frame)
        .on_upgrade(move |socket| run_browser_ws(socket, st, session_id))
        .into_response()
}

/// Runs the browser WebSocket relay loop for a terminal session with bounded queues and slow-reader timeouts.
async fn run_browser_ws(mut socket: WebSocket, st: AppState, session_id: String) {
    // Subscribing and taking what came earlier happen under one lock, so no frame is missed or sent twice.
    let init = {
        let mut sessions = st.terminals.sessions.locked();
        sessions.get_mut(&session_id).map(|s| {
            s.attached = true;
            (
                s.browser_tx.subscribe(),
                s.last_ready.clone(),
                s.last_state.clone(),
                std::mem::take(&mut s.early),
            )
        })
    };
    let Some((mut rx, last_ready, last_state, early)) = init else {
        return;
    };

    for initial in [last_ready, last_state].into_iter().flatten().chain(early) {
        if let Ok(text) = serde_json::to_string(&initial)
            && tokio::time::timeout(BROWSER_SEND_STALL, socket.send(Message::Text(text.into())))
                .await
                .is_err()
        {
            return;
        }
    }

    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.tick().await;

    loop {
        tokio::select! {
            biased;
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(t))) => {
                    let Ok(msg) = serde_json::from_str::<BrowserWsIn>(t.as_str()) else {
                        continue;
                    };
                    let now = now_ms();
                    let now_s = now / 1000;
                    let mut expired_or_revoked = false;
                    let mut node_and_frame = None;
                    {
                        let mut sessions = st.terminals.sessions.locked();
                        let Some(s) = sessions.get_mut(&session_id) else {
                            break;
                        };
                        if s.revoked || now > s.expires_at_ms {
                            expired_or_revoked = true;
                        } else {
                            let node = s.node.clone();
                            let frame_opt = match msg.kind.as_str() {
                                "term.frame" => {
                                    if let Some(payload) = msg.payload.filter(|p| !p.is_empty() && p.len() <= 262_144) {
                                        st.terminals.metrics.frames_relayed.fetch_add(1, Ordering::Relaxed);
                                        st.terminals.metrics.bytes_relayed.fetch_add(payload.len() as u64, Ordering::Relaxed);
                                        Some(HubFrame::TermFrame {
                                            session_id: session_id.clone(),
                                            generation: msg.generation.unwrap_or(s.generation),
                                            payload,
                                        })
                                    } else {
                                        None
                                    }
                                }
                                "term.ice" => msg.candidate.map(|candidate| HubFrame::TermIce {
                                    session_id: session_id.clone(),
                                    candidate,
                                }),
                                "term.offer" => {
                                    s.generation += 1;
                                    s.jti = format!("jti-{}", &random_hex()[..24]);
                                    s.expires_at_ms = (now_s + TERMINAL_GRANT_TTL_SECS) * 1000;
                                    let claims = TerminalGrantClaims {
                                        jti: s.jti.clone(),
                                        sub: s.user_id.clone(),
                                        aud: s.node.clone(),
                                        prj: s.project.clone(),
                                        agt: s.agent_id.clone(),
                                        generation: s.generation,
                                        read: s.read,
                                        write: s.write,
                                        iat: now_s,
                                        exp: now_s + TERMINAL_GRANT_TTL_SECS,
                                        dtls_fp: None,
                                    };
                                    let node_key = st.terminals.node_grant_key(&s.node);
                                    if let Some(new_grant) = sign_terminal_grant(&claims, &node_key) {
                                        s.grant = new_grant;
                                    }
                                    Some(HubFrame::TermOpen {
                                        session_id: session_id.clone(),
                                        agent_id: s.agent_id.clone(),
                                        grant: s.grant.clone(),
                                        generation: s.generation,
                                        transport: msg.transport.unwrap_or_else(|| "auto".into()),
                                        offer_sdp: msg.offer_sdp,
                                        e2e_key: (!s.e2e_key.is_empty()).then(|| s.e2e_key.clone()),
                                        ice_servers: st.terminals.ice_servers.clone(),
                                        cols: msg.cols.unwrap_or(120).clamp(20, 500),
                                        rows: msg.rows.unwrap_or(36).clamp(5, 250),
                                    })
                                }
                                "term.close" => Some(HubFrame::TermClose {
                                    session_id: session_id.clone(),
                                    jti: None,
                                    reason: msg.reason.unwrap_or_else(|| "browser closed terminal".into()),
                                }),
                                _ => None,
                            };
                            if let Some(frame) = frame_opt {
                                node_and_frame = Some((node, frame));
                            }
                        }
                    }
                    if expired_or_revoked {
                        let _ = socket.send(Message::Close(Some(CloseFrame {
                            code: 4003,
                            reason: "grant expired or revoked".into(),
                        }))).await;
                        break;
                    }
                    if let Some((node, frame)) = node_and_frame {
                        let _ = st.handle.call(move |c, _| {
                            let mut fx = Vec::new();
                            if let Some(conn) = c.conn_of(&node) {
                                fx.push(Effect::Send { conn, frame });
                            }
                            ((), fx)
                        }).await;
                    }
                }
                _ => {}
            },
            out = rx.recv() => match out {
                Ok(node_frame) => {
                    let is_revoked = matches!(
                        &node_frame,
                        NodeFrame::TermState { state, .. } if state == "revoked"
                    );
                    if let Ok(text) = serde_json::to_string(&node_frame) {
                        match tokio::time::timeout(
                            BROWSER_SEND_STALL,
                            socket.send(Message::Text(text.into())),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            _ => {
                                st.terminals
                                    .metrics
                                    .slow_reader_drops
                                    .fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    if is_revoked {
                        let _ = socket
                            .send(Message::Close(Some(CloseFrame {
                                code: 4003,
                                reason: "session revoked".into(),
                            })))
                            .await;
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    st.terminals
                        .metrics
                        .slow_reader_drops
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = ping.tick() => {
                if tokio::time::timeout(
                    BROWSER_SEND_STALL,
                    socket.send(Message::Ping(Vec::new().into())),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Returns terminal streaming metrics as JSON for an authorized viewer.
pub(super) async fn metrics_for(st: &AppState, _who: Who) -> Response {
    reply_json(StatusCode::OK, st.terminals.metrics_json(now_ms()))
}

// ----- Single-tenant hub handlers (`State<AppState>`) -----

/// `POST /api/v1/terminal/sessions` on a single-tenant hub.
pub(super) async fn create_session(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionReq>,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return denied();
    };
    create_session_for(&st, who, req).await
}

/// `POST /api/v1/terminal/sessions/{id}/signal` on a single-tenant hub.
pub(super) async fn signal_session(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SignalSessionReq>,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return denied();
    };
    signal_session_for(&st, who, id, req).await
}

/// `POST /api/v1/terminal/sessions/{id}/revoke` on a single-tenant hub.
pub(super) async fn revoke_session(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<RevokeSessionReq>>,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        st.terminals
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        return denied();
    };
    revoke_session_for(&st, who, id, body.map(|b| b.0).unwrap_or_default()).await
}

/// `GET /api/v1/terminal/ws` on a single-tenant hub.
pub(super) async fn ws_upgrade(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let who_opt = authorised(&st, peer, &headers).await;
    ws_for(st, who_opt, query, ws).await
}

/// `GET /api/v1/terminal/metrics` on a single-tenant hub.
pub(super) async fn metrics(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        return denied();
    };
    metrics_for(&st, who).await
}

// ----- Multi-tenant hosted gateway handlers (`State<Gateway>`) -----

/// `POST /api/v1/terminal/sessions` on the hosted gateway.
pub(super) async fn gw_create_session(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionReq>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return reply_json(StatusCode::UNAUTHORIZED, json!({"error": "sign in first"}));
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return reply_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "your hub could not start"}),
        );
    };
    create_session_for(&hub.state, Who::Everything, req).await
}

/// `POST /api/v1/terminal/sessions/{id}/signal` on the hosted gateway.
pub(super) async fn gw_signal_session(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SignalSessionReq>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return reply_json(StatusCode::UNAUTHORIZED, json!({"error": "sign in first"}));
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return reply_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "your hub could not start"}),
        );
    };
    signal_session_for(&hub.state, Who::Everything, id, req).await
}

/// `POST /api/v1/terminal/sessions/{id}/revoke` on the hosted gateway.
pub(super) async fn gw_revoke_session(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<RevokeSessionReq>>,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return reply_json(StatusCode::UNAUTHORIZED, json!({"error": "sign in first"}));
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return reply_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "your hub could not start"}),
        );
    };
    revoke_session_for(
        &hub.state,
        Who::Everything,
        id,
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}

/// `GET /api/v1/terminal/ws` on the hosted gateway.
pub(super) async fn gw_ws_upgrade(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return reply_json(StatusCode::UNAUTHORIZED, json!({"error": "sign in first"}));
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return reply_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "your hub could not start"}),
        );
    };
    ws_for(hub.state.clone(), Some(Who::Everything), query, ws).await
}

/// `GET /api/v1/terminal/metrics` on the hosted gateway.
pub(super) async fn gw_metrics(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(account) = account_of(&gw, &headers) else {
        return reply_json(StatusCode::UNAUTHORIZED, json!({"error": "sign in first"}));
    };
    let Ok(hub) = gw.registry.hub(&account) else {
        return reply_json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "your hub could not start"}),
        );
    };
    metrics_for(&hub.state, Who::Everything).await
}
