//! What a prober, a load balancer and the dashboard ask the hub about its own health.
//!   /healthz  the process answers (shallow; for "is it running at all")
//!   /readyz   the hub can do its job: its core answers in time, its database reads, and, when Discord is part of this hub,
//!             the Discord gateway is connected. 200 when all pass, 503 with the failing checks named otherwise.
//!   /metrics  Prometheus text (needs a dashboard token): availability, machines, agents.
//!   /api/v1/uptime  availability and error budget per component, for the dashboard (needs sign-in or a token).

use super::AppState;
use super::web::{Who, authorised, denied, secure};
use crate::uptime::{self, State as Up};
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;

const HOUR: i64 = 3_600_000;
/// The windows reported: name and length.
pub const WINDOWS: [(&str, i64); 4] = [
    ("1h", HOUR),
    ("24h", 24 * HOUR),
    ("7d", 7 * 24 * HOUR),
    ("30d", 30 * 24 * HOUR),
];

/// Readiness: the checks, whether all passed.
pub(super) async fn readyz(State(st): State<AppState>) -> Response {
    let core = tokio::time::timeout(Duration::from_secs(2), st.handle.call(|_, _| ((), vec![])))
        .await
        .is_ok_and(|r| r.is_some());
    let db = st
        .reader
        .lock()
        .expect("lock")
        .as_ref()
        .is_none_or(|s| s.kv_get("hub_alive_at").is_ok());
    let discord = !st.cfg.discord_expected
        || st
            .reader
            .lock()
            .expect("lock")
            .as_ref()
            .is_some_and(|s| s.uptime_last("discord").ok().flatten() == Some(Up::Up));
    let ok = core && db && discord;
    let body = json!({"ok": ok, "checks": {"core": core, "database": db, "discord": discord}});
    secure(
        (
            if ok {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            Json(body),
        )
            .into_response(),
        "application/json",
        "no-store",
    )
}

/// Availability and budget per component over every window.
fn summary(st: &AppState) -> Value {
    let now = crate::now_ms();
    let guard = st.reader.lock().expect("lock");
    let Some(db) = guard.as_ref() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for c in db.uptime_components().unwrap_or_default() {
        let mut windows = serde_json::Map::new();
        for (name, len) in WINDOWS {
            let changes = db.uptime_changes(&c, now - len).unwrap_or_default();
            let r = uptime::report(&changes, now - len, now);
            windows.insert(
                name.into(),
                json!({
                    "availability": r.availability(),
                    "downMs": r.down_ms,
                    "budgetLeftMs": uptime::budget_left(&r, st.cfg.uptime_target),
                }),
            );
        }
        let state = db
            .uptime_last(&c)
            .ok()
            .flatten()
            .map_or("unknown", Up::as_str);
        let recent: Vec<Value> = uptime::outages(
            &db.uptime_changes(&c, now - 7 * 24 * HOUR)
                .unwrap_or_default(),
            now - 7 * 24 * HOUR,
            now,
        )
        .iter()
        .rev()
        .take(5)
        .map(|(a, b)| json!({"from": a, "to": b}))
        .collect();
        out.insert(
            c,
            json!({"state": state, "windows": windows, "outages": recent}),
        );
    }
    json!({"target": st.cfg.uptime_target, "components": out})
}

pub(super) async fn uptime(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if authorised(&st, peer, &headers).await.is_none() {
        return denied();
    }
    secure(
        Json(summary(&st)).into_response(),
        "application/json",
        "no-store",
    )
}

/// Prometheus text. Only a dashboard token (not a Discord sign-in) opens it, since a scraper holds a token.
pub(super) async fn metrics(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !matches!(authorised(&st, peer, &headers).await, Some(Who::Everything)) {
        return denied();
    }
    let (devices, agents) = st
        .handle
        .call(|c, _| {
            (
                (
                    c.devices().iter().filter(|d| d.connected).count(),
                    c.projects()
                        .iter()
                        .map(|p| c.agents_of_project(p).len())
                        .sum::<usize>(),
                ),
                vec![],
            )
        })
        .await
        .unwrap_or_default();
    let mut text = format!(
        "# TYPE claudecord_machines_connected gauge\nclaudecord_machines_connected {devices}\n# TYPE claudecord_agents gauge\nclaudecord_agents {agents}\n# TYPE claudecord_component_up gauge\n# TYPE claudecord_availability_ratio gauge\n"
    );
    if let Some(components) = summary(&st)["components"].as_object() {
        for (name, c) in components {
            text.push_str(&format!(
                "claudecord_component_up{{component=\"{name}\"}} {}\n",
                u8::from(c["state"] == "up")
            ));
            for (w, v) in c["windows"].as_object().into_iter().flatten() {
                if let Some(a) = v["availability"].as_f64() {
                    text.push_str(&format!("claudecord_availability_ratio{{component=\"{name}\",window=\"{w}\"}} {a:.6}\n"));
                }
            }
        }
    }
    secure(
        text.into_response(),
        "text/plain; version=0.0.4",
        "no-store",
    )
}
