//! The dashboard: a read-only web page that shows machines, agents, what is waiting on a person, tasks and the conversation,
//! served by the hub itself (no separate program, nothing to install). The page and its script are fixed files that hold no
//! data. The data comes from two endpoints that need a dashboard token (made with `claudecord web-token`), checked the same
//! way a machine's token is. A machine's token never opens the dashboard and a dashboard token never connects a machine.
//!
//! Everything shown can be written by an agent, so the script draws it as text only, and the page is served with a strict
//! content policy that allows scripts only from the hub itself.

use super::{AppState, Input};
use axum::{
    Json,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::oneshot;

const INDEX: &str = include_str!("web/index.html");
const SCRIPT: &str = include_str!("web/app.js");
const STYLE: &str = include_str!("web/app.css");

/// The prefix that marks a token as a dashboard token rather than a machine's.
pub const WEB_PREFIX: &str = "web:";

/// Headers every response carries: no guessing at types, no referrer, and scripts only from this origin.
fn secure(mut r: Response, content_type: &'static str, cache: &'static str) -> Response {
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    r
}

/// The page itself.
pub async fn index() -> Response {
    secure(
        INDEX.into_response(),
        "text/html; charset=utf-8",
        "no-cache",
    )
}

/// The page's script.
pub async fn script() -> Response {
    secure(
        SCRIPT.into_response(),
        "text/javascript; charset=utf-8",
        "no-cache",
    )
}

/// The page's style.
pub async fn style() -> Response {
    secure(STYLE.into_response(), "text/css; charset=utf-8", "no-cache")
}

/// Checks the dashboard token on a request. Repeated failures from one address are blocked, like for machines.
async fn authorised(st: &AppState, peer: SocketAddr, headers: &HeaderMap) -> bool {
    let ip = peer.ip().to_string();
    let now = crate::now_ms() as f64;
    if st.failures.lock().expect("lock").blocked(&ip, now) {
        return false;
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
    if node.as_deref().is_some_and(|n| n.starts_with(WEB_PREFIX)) {
        true
    } else {
        st.failures.lock().expect("lock").fail(&ip, now);
        false
    }
}

fn denied() -> Response {
    secure(
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "a dashboard token is needed"})),
        )
            .into_response(),
        "application/json",
        "no-store",
    )
}

/// Everything the dashboard shows apart from the conversation: machines, and per project its agents, what is waiting on a
/// person, and its tasks.
pub(super) async fn state(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !authorised(&st, peer, &headers).await {
        return denied();
    }
    let body = st
        .handle
        .call(|c, _| {
            let devices: Vec<Value> = c.devices().iter().map(|d| json!({"node": d.node, "connected": d.connected, "agents": d.agents.len(), "max": d.max_agents, "labels": d.labels, "lastSeen": d.last_seen})).collect();
            let mut projects = serde_json::Map::new();
            for p in c.projects() {
                let agents: Vec<Value> = c.agents_of_project(&p).iter().map(|a| json!({"name": a.name, "lead": a.is_lead, "node": a.node_name, "status": format!("{:?}", c.status_of(&a.agent_id)).to_lowercase()})).collect();
                let name_of = |id: &str| c.agent(id).map_or(id.to_string(), |a| a.name.clone());
                let asks: Vec<Value> = c.asks_of(&p).iter().filter(|a| a.state == crate::hub::AskState::Open).map(|a| json!({"id": format!("Q{}", a.qn), "agent": name_of(&a.agent_id), "question": a.question})).collect();
                let perms: Vec<Value> = c.perms_of(&p).iter().filter(|x| x.state == crate::hub::PermState::Open).map(|x| json!({"id": format!("P{}", x.pn), "agent": name_of(&x.agent_id), "kind": x.kind, "action": x.action})).collect();
                let tasks: Vec<Value> = c.tasks_of(&p).iter().map(|t| json!({"id": t.id, "to": name_of(&t.to_agent), "text": t.text, "state": format!("{:?}", t.state).to_lowercase()})).collect();
                projects.insert(p, json!({"agents": agents, "asks": asks, "perms": perms, "tasks": tasks}));
            }
            (json!({"devices": devices, "projects": projects}), vec![])
        })
        .await
        .unwrap_or(Value::Null);
    secure(Json(body).into_response(), "application/json", "no-store")
}

/// The conversation of a project: the newest rows, or rows after a given id, optionally one thread.
pub(super) async fn history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authorised(&st, peer, &headers).await {
        return denied();
    }
    let Some(project) = q.get("project") else {
        return secure(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "project is required"})),
            )
                .into_response(),
            "application/json",
            "no-store",
        );
    };
    let limit = q
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let thread = q.get("thread").map(String::as_str);
    let rows = {
        let guard = st.reader.lock().expect("lock");
        guard.as_ref().and_then(|s| {
            if q.contains_key("latest") {
                s.history_latest(project, thread, limit).ok()
            } else {
                s.history(
                    project,
                    thread,
                    q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0),
                    limit,
                )
                .ok()
            }
        })
    }
    .unwrap_or_default();
    let out: Vec<Value> = rows.iter().map(|r| json!({"id": r.id, "at": r.at, "from": r.from, "kind": r.kind, "thread": r.thread, "text": r.text})).collect();
    secure(Json(out).into_response(), "application/json", "no-store")
}
