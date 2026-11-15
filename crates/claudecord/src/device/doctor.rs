//! `claudecord doctor`: checks, step by step, whether this machine can reach the hub, and says plainly where it stops.
//! Locked-down servers fail in different places (no route, a proxy that needs a login, a firewall that cuts idle
//! connections), and the fix depends on which, so each step reports on its own.

use super::config::Config;
use super::link::{LinkOpts, Ws, connect_detailed};
use crate::protocol::HubFrame;
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

/// How to install the program of an agent type, for the message that says it is missing.
pub fn install_hint(program: &str) -> &'static str {
    match program {
        "codex" => "npm i -g @openai/codex",
        "agy" => "install Antigravity and put `agy` on your PATH",
        other => match crate::protocol::HARNESSES
            .iter()
            .find(|h| h.program == other)
        {
            Some(h) => h.install,
            None => "npm i -g @anthropic-ai/claude-code   (see https://claude.com/claude-code)",
        },
    }
}

/// What to say when `program` is not on the PATH: what is missing and how to get it.
pub fn program_missing(program: &str) -> String {
    format!(
        "{program} was not found on this machine's PATH. Install it with:  {}",
        install_hint(program)
    )
}

/// What to say when tmux is needed and missing, or None when it is there (or not needed: Windows, or the built-in terminal asked for).
pub fn tmux_missing() -> Option<String> {
    let wants_pty = std::env::var("CLAUDECORD_TERMINAL").is_ok_and(|v| v == "pty");
    if cfg!(windows) || wants_pty || super::tmux::TmuxTerminal::available() {
        return None;
    }
    let how = if cfg!(target_os = "macos") {
        "brew install tmux"
    } else {
        "sudo apt install tmux   (or: dnf install tmux, pacman -S tmux)"
    };
    Some(format!(
        "tmux was not found. It keeps each agent running when you close the window. Install it with:  {how}"
    ))
}

/// The program an agent type runs.
pub fn program_of(adapter: &str) -> &'static str {
    match adapter {
        "codex" => "codex",
        "agy" => "agy",
        other => crate::protocol::harness(other).map_or("claude", |h| h.program),
    }
}

/// Whether this version (`mine`) is still welcome at a hub that is `hub` and serves nothing older than `min`. Too old to be served is a failure;
/// merely older than the hub is a note that an update exists.
fn version_check(mine: &str, hub: Option<&str>, min: Option<&str>, how: &str) -> Check {
    use crate::protocol::version_older;
    if let Some(min) = min.filter(|m| version_older(mine, m)) {
        return check(
            "version",
            false,
            format!(
                "this is {mine} and the hub only serves {min} or newer, so it will refuse this machine. Update with:  {how}"
            ),
        );
    }
    match hub {
        Some(h) if version_older(mine, h) => check(
            "version",
            true,
            format!("update available: the hub is {h}, this is {mine}. Update with:  {how}"),
        ),
        Some(h) => check("version", true, format!("{mine} (the hub is {h})")),
        None => check(
            "version",
            true,
            format!("{mine} (the hub did not say its version)"),
        ),
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
    let auth = super::link::Auth::new(
        &cfg.hub_url,
        cfg.token.clone(),
        // The saved login is kept up to date only if this is the saved one.
        Some(super::config::home_dir())
            .filter(|d| super::config::Config::load(d).as_ref() == Some(cfg)),
    );
    let connected = match auth.bearer().await {
        Ok(token) => connect_detailed(&cfg.connect_url(), &token, opts).await,
        Err(e) => Err(e),
    };
    let mut ws: Ws = match connected {
        Ok(ws) => ws,
        Err(e) => {
            // A refused token is the hub not knowing this machine any more: say how to fix it, not only what the handshake said.
            let hint = if e.contains("401") {
                " (the hub does not recognise this machine's login; approve it again with: claudecord login)"
            } else {
                ""
            };
            out.push(check("connect", false, format!("{e}{hint}")));
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
        Ok(Some(Ok(Message::Text(t)))) if t.contains("welcome") => {
            out.push(check(
                "welcome",
                true,
                "the hub accepted this machine's token",
            ));
            // The welcome says which version the hub is and the oldest it serves, so say here whether this one is still welcome.
            if let Some(HubFrame::Welcome {
                hub_version,
                min_version,
                ..
            }) = HubFrame::parse(t.as_str())
            {
                let exe = std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push(version_check(
                    env!("CARGO_PKG_VERSION"),
                    hub_version.as_deref(),
                    min_version.as_deref(),
                    crate::cli::machine::update_command(&exe),
                ));
            }
        }
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

#[cfg(test)]
mod requirement_tests {
    use super::*;

    #[test]
    fn a_missing_program_says_what_and_how_to_get_it() {
        assert!(program_missing("codex").contains("npm i -g @openai/codex"));
        assert!(program_missing("claude").contains("claude-code"));
        assert!(program_missing("agy").contains("Antigravity"));
        assert_eq!(program_of("codex"), "codex");
        assert_eq!(program_of("whatever"), "claude");
    }

    #[test]
    fn a_version_the_hub_still_serves_is_fine_an_older_than_hub_one_is_told_to_update_and_one_below_the_minimum_fails()
     {
        let how = "npm i -g claudecord@latest";
        let up = version_check("0.5.1", Some("0.5.1"), Some("0.1.0"), how);
        assert!(up.ok && up.detail.contains("0.5.1"), "{}", up.detail);
        let old = version_check("0.4.2", Some("0.5.1"), Some("0.1.0"), how);
        assert!(old.ok, "still served: an update exists, nothing is refused");
        assert!(old.detail.contains("update available") && old.detail.contains(how));
        let refused = version_check("0.4.2", Some("0.5.1"), Some("0.5.0"), how);
        assert!(
            !refused.ok && refused.detail.contains("will refuse") && refused.detail.contains(how)
        );
        let unknown = version_check("0.5.1", None, None, how);
        assert!(unknown.ok && unknown.detail.contains("did not say"));
    }
}
