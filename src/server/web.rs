//! The dashboard: a read-only web page that shows machines, agents, what is waiting on a person, tasks and the conversation,
//! served by the hub itself (no separate program, nothing to install). The page and its script are fixed files that hold no
//! data. The data comes from two endpoints that need a dashboard token (made with `claudecord web-token`), checked the same
//! way a machine's token is. A machine's token never opens the dashboard and a dashboard token never connects a machine.
//!
//! Everything shown can be written by an agent, so the script draws it as text only, and the page is served with a strict
//! content policy that allows scripts only from the hub itself.

use super::{AppState, Input};
use crate::sync::Lock;
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
pub(super) fn secure(mut r: Response, content_type: &'static str, cache: &'static str) -> Response {
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

/// Who is looking at the dashboard: someone holding a dashboard token sees everything, someone signed in with Discord sees
/// the projects their account has a role in.
pub(super) enum Who {
    Everything,
    Account(String),
}

impl Who {
    /// Whether this viewer may see a project's data, given the core.
    fn may_see(&self, c: &crate::hub::HubCore, project: &str) -> bool {
        match self {
            Who::Everything => true,
            Who::Account(id) => c.role_of(project, id).is_some(),
        }
    }
}

/// Works out who is asking: a Discord sign-in, or a dashboard token. Repeated bad tokens from one address are blocked, like for
/// machines. Having no credentials at all is not a failure (the page asks before it has any).
pub(super) async fn authorised(
    st: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<Who> {
    if let Some((id, _)) = super::login::who(st, headers) {
        return Some(Who::Account(id));
    }
    let ip = peer.ip().to_string();
    let now = crate::now_ms() as f64;
    if st.failures.locked().blocked(&ip, now) {
        return None;
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return None;
    }
    let (reply, answer) = oneshot::channel();
    let node = if st
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
        Some(Who::Everything)
    } else {
        st.failures.locked().fail(&ip, now);
        None
    }
}

pub(super) fn denied() -> Response {
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

/// The dashboard's state as JSON, for whoever `who` says is looking.
pub(super) async fn state_json(st: &AppState, who: Who) -> Value {
    st
        .handle
        .call(move |c, _| {
            let devices: Vec<Value> = c.devices().iter().map(|d| json!({"node": d.node, "connected": d.connected, "agents": d.agents.len(), "max": d.max_agents, "labels": d.labels, "lastSeen": d.last_seen})).collect();
            let mut projects = serde_json::Map::new();
            for p in c.projects().into_iter().filter(|p| who.may_see(c, p)) {
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
        .unwrap_or(Value::Null)
}

/// Everything the dashboard shows apart from the conversation: machines, and per project its agents, what is waiting on a
/// person, and its tasks.
pub(super) async fn state(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        return denied();
    };
    let body = state_json(&st, who).await;
    secure(Json(body).into_response(), "application/json", "no-store")
}

/// The conversation of a project: the newest rows, or rows after a given id, optionally one thread.
pub(super) async fn history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        return denied();
    };
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
    // A signed-in person may read only the projects they have a role in, checked now so removing them takes effect at once.
    let project_name = project.clone();
    if !st
        .handle
        .call(move |c, _| (who.may_see(c, &project_name), vec![]))
        .await
        .unwrap_or(false)
    {
        return secure(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "no access to this project"})),
            )
                .into_response(),
            "application/json",
            "no-store",
        );
    }
    let limit = q
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let thread = q.get("thread").map(String::as_str);
    // The read goes to a thread meant for blocking work, so a big history query never holds up the connections.
    let reader = st.reader.clone();
    let (project, thread) = (project.clone(), thread.map(String::from));
    let (latest, after) = (
        q.contains_key("latest"),
        q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0),
    );
    let before: Option<i64> = q.get("before").and_then(|b| b.parse().ok());
    let rows = tokio::task::spawn_blocking(move || {
        let guard = reader.locked();
        let s = guard.as_ref()?;
        if before.is_some() {
            // Paging back: older than the oldest row the page has, from the database and then the compressed files.
            s.history_before(&project, thread.as_deref(), before, limit)
                .ok()
        } else if latest {
            s.history_latest(&project, thread.as_deref(), limit).ok()
        } else {
            s.history(&project, thread.as_deref(), after, limit).ok()
        }
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let out: Vec<Value> = rows.iter().map(|r| json!({"id": r.id, "at": r.at, "from": r.from, "kind": r.kind, "thread": r.thread, "text": r.text})).collect();
    secure(Json(out).into_response(), "application/json", "no-store")
}
