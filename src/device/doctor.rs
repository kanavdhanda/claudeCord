//! `claudecord doctor`: checks, step by step, whether this machine can reach the hub, and says plainly where it stops.
//! Locked-down servers fail in different places (no route, a proxy that needs a login, a firewall that cuts idle
//! connections), and the fix depends on which, so each step reports on its own.

use super::config::Config;
use super::link::{LinkOpts, Ws, connect_detailed};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// The result of one check.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
    }
}

/// Runs the checks in order and stops at the first failure, since later ones depend on earlier ones.
pub async fn run(cfg: &Config, opts: &LinkOpts) -> Vec<Check> {
    let mut out = Vec::new();
    let proxy = opts.proxy.clone().or_else(|| {
        [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
        ]
        .iter()
        .find_map(|n| std::env::var(n).ok())
    });
    out.push(check(
        "route",
        true,
        proxy.as_ref().map_or(
            "direct (outbound only, no ports to open)".to_string(),
            |p| format!("through proxy {}", p.split('@').next_back().unwrap_or(p)),
        ),
    ));
    let started = Instant::now();
    let mut ws: Ws = match connect_detailed(&cfg.connect_url(), &cfg.token, opts).await {
        Ok(ws) => ws,
        Err(e) => {
            out.push(check("connect", false, e));
            return out;
        }
    };
    out.push(check(
        "connect",
        true,
        format!("{} ms", started.elapsed().as_millis()),
    ));
    // The hub welcomes a device it accepts. A bad token never gets this far, it is refused during connect.
    match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) if t.contains("welcome") => out.push(check(
            "welcome",
            true,
            "the hub accepted this machine's token",
        )),
        _ => {
            out.push(check(
                "welcome",
                false,
                "connected but the hub did not say welcome",
            ));
            return out;
        }
    }
    let sent = Instant::now();
    if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
        out.push(check("heartbeat", false, "could not send"));
        return out;
    }
    let pong = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(m)) = ws.next().await {
            if matches!(m, Message::Pong(_)) {
                return true;
            }
        }
        false
    })
    .await;
    match pong {
        Ok(true) => out.push(check(
            "heartbeat",
            true,
            format!("round trip {} ms", sent.elapsed().as_millis()),
        )),
        _ => out.push(check(
            "heartbeat",
            false,
            "no answer: something between here and the hub may be dropping the connection",
        )),
    }
    out
}
