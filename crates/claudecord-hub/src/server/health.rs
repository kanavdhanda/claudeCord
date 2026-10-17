//! What a prober, a load balancer and the dashboard ask the hub about its own health.
//!   /healthz  the process answers (shallow; for "is it running at all")
//!   /readyz   the hub can do its job: its core answers in time, its database reads, and, when Discord is part of this hub,
//!             the Discord gateway is connected. 200 when all pass, 503 with the failing checks named otherwise.
//!   /metrics  Prometheus text (needs a dashboard token): availability, machines, agents.
//!   /api/v1/uptime  availability and error budget per component, for the dashboard (needs sign-in or a token).
//!   /api/v1/logs    the end of the hub's log, filtered (a dashboard token, or a signed-in workspace owner; logs name machines and addresses).

use super::AppState;
use super::web::{Who, authorised, denied, secure};
use crate::store::Store;
use crate::sync::Lock;
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
    let reader = st.reader.clone();
    let discord_expected = st.cfg.discord_expected;
    let (db, discord) = tokio::task::spawn_blocking(move || {
        let guard = reader.locked();
        let db = guard
            .as_ref()
            .is_none_or(|s| s.kv_get("hub_alive_at").is_ok());
        let discord = !discord_expected
            || guard
                .as_ref()
                .is_some_and(|s| s.uptime_last("discord").ok().flatten() == Some(Up::Up));
        (db, discord)
    })
    .await
    .unwrap_or((false, false));
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
/// The same, worked out on a thread meant for blocking work.
pub(super) async fn summary(st: &AppState) -> Value {
    let (reader, target) = (st.reader.clone(), st.cfg.uptime_target);
    tokio::task::spawn_blocking(move || summary_blocking(&reader, target))
        .await
        .unwrap_or_default()
}

fn summary_blocking(
    reader: &std::sync::Arc<std::sync::Mutex<Option<Store>>>,
    target: f64,
) -> Value {
    let now = crate::now_ms();
    let guard = reader.locked();
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
                    "budgetLeftMs": uptime::budget_left(&r, target),
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
    json!({"target": target, "components": out})
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
        Json(summary(&st).await).into_response(),
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
    if let Some(components) = summary(&st).await["components"].as_object() {
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

/// How far back from the end of a log file a request looks: enough for thousands of lines, never the whole of a big file.
const LOG_TAIL_BYTES: u64 = 512 * 1024;

/// The last lines of a log file that pass the filters, oldest first. `min` keeps lines at that level or worse.
fn log_tail(
    path: &std::path::Path,
    min: crate::log::Level,
    needle: &str,
    max: usize,
) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let read_end = |p: &std::path::Path| -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(p) else {
            return Vec::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        let start = len.saturating_sub(LOG_TAIL_BYTES);
        let _ = f.seek(SeekFrom::Start(start));
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        // Starting in the middle of the file, the first line is probably a piece of one.
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        lines
    };
    // The file before the last rotation comes first, so a rotation just now does not empty the view.
    let mut all = read_end(&path.with_extension("log.1"));
    all.extend(read_end(path));
    let needle = needle.to_lowercase();
    let level_of = |line: &str| {
        line.split_whitespace()
            .nth(3)
            .and_then(crate::log::Level::parse)
    };
    let mut keep: Vec<String> = all
        .into_iter()
        .filter(|l| level_of(l).is_none_or(|lv| lv <= min))
        .filter(|l| needle.is_empty() || l.to_lowercase().contains(&needle))
        .collect();
    let skip = keep.len().saturating_sub(max);
    keep.drain(..skip);
    keep
}

/// The end of the hub's log. Query: `lines` (default 200, at most 1000), `level` (error, warn, info or debug: that level and worse) and
/// `q` (a word to look for). Lines are already scrubbed of secrets when written. Only a dashboard token or a workspace owner may read it.
pub(super) async fn logs(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(who) = authorised(&st, peer, &headers).await else {
        return denied();
    };
    let allowed = match who {
        Who::Everything => true,
        Who::Account(id) => st
            .handle
            .call(move |c, _| (c.role_of("", &id) == Some(crate::hub::Role::Owner), vec![]))
            .await
            .unwrap_or(false),
    };
    if !allowed {
        return secure(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "only a workspace owner may read the hub's log"})),
            )
                .into_response(),
            "application/json",
            "no-store",
        );
    }
    let Some(path) = st.cfg.log_path.clone() else {
        return secure(
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "this hub is not writing a log file"})),
            )
                .into_response(),
            "application/json",
            "no-store",
        );
    };
    let max = q
        .get("lines")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let level = q
        .get("level")
        .and_then(|l| crate::log::Level::parse(l))
        .unwrap_or(crate::log::Level::Debug);
    let needle = q.get("q").cloned().unwrap_or_default();
    let lines = tokio::task::spawn_blocking(move || log_tail(&path, level, &needle, max))
        .await
        .unwrap_or_default();
    if q.get("format").is_some_and(|f| f == "text") {
        return secure(
            lines.join("\n").into_response(),
            "text/plain; charset=utf-8",
            "no-store",
        );
    }
    secure(
        Json(json!({ "lines": lines })).into_response(),
        "application/json",
        "no-store",
    )
}
