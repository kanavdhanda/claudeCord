//! A machine joining a hub with no token typed by anyone: it asks the hub for a short code, shows the person a link, and waits while they
//! sign in with Discord in a browser and approve it. This is what the very first `claudecord start` (or `npx claudecord`) does on a new
//! machine. The token it gets back never appears on screen or in a shell history; it goes straight into the machine's private config.

use super::config::Config;
use serde_json::{Value, json};
use std::time::Duration;

/// Where machines go when nothing else is said: the address this build was made for (`CLAUDECORD_HUB` at build time), so a downloaded
/// program already knows its home.
pub const DEFAULT_HUB: &str = match option_env!("CLAUDECORD_HUB") {
    Some(h) => h,
    None => "https://claudecord.example.com",
};

/// The `http(s)` form of a hub address, whatever form it was given in (`wss://x`, `https://x`, or just `x`).
pub fn http_base(hub: &str) -> String {
    let h = hub.trim().trim_end_matches('/');
    if let Some(r) = h.strip_prefix("wss://") {
        format!("https://{r}")
    } else if let Some(r) = h.strip_prefix("ws://") {
        format!("http://{r}")
    } else if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        format!("https://{h}")
    }
}

/// The `ws(s)` form, which is what a saved machine config keeps.
pub fn ws_base(hub: &str) -> String {
    let h = http_base(hub);
    if let Some(r) = h.strip_prefix("https://") {
        format!("wss://{r}")
    } else {
        format!("ws://{}", h.trim_start_matches("http://"))
    }
}

/// This machine's name when nobody chose one: its hostname.
pub fn default_node_name() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .unwrap_or_else(|| "this-machine".into());
    let clean: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let clean = clean.trim_start_matches(['.', '-', '_']).to_string();
    if clean.is_empty() {
        "this-machine".into()
    } else {
        clean
    }
}

/// Opens the link in the person's browser if it can. Silent if it cannot: the link is printed anyway.
pub fn open_browser(url: &str) {
    if std::env::var_os("CLAUDECORD_NO_BROWSER").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url.to_string()]);
    #[cfg(target_os = "windows")]
    let cmd = (
        "cmd",
        vec!["/c".into(), "start".into(), "".into(), url.to_string()],
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = ("xdg-open", vec![url.to_string()]);
    let _ = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Asks the hub for a code, tells the caller what to show (`show(user_code, link)`), and waits for a person to approve. Returns the
/// config to save. Gives up when the code runs out or the hub says it is no longer valid.
pub async fn enroll(hub: &str, node: &str, show: impl Fn(&str, &str)) -> Result<Config, String> {
    let base = http_base(hub);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let r = http
        .post(format!("{base}/api/device/code"))
        .json(&json!({ "node": node }))
        .send()
        .await
        .map_err(|e| format!("cannot reach {base}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{base} refused the request ({})", r.status()));
    }
    let c: Value = r.json().await.map_err(|e| e.to_string())?;
    let (device, user, link) = (
        c["device_code"]
            .as_str()
            .ok_or("the hub sent no code")?
            .to_string(),
        c["user_code"].as_str().unwrap_or("").to_string(),
        c["verify_url"].as_str().unwrap_or("").to_string(),
    );
    let every = Duration::from_secs(c["interval"].as_u64().unwrap_or(3).clamp(1, 30));
    let until =
        std::time::Instant::now() + Duration::from_secs(c["expires_in"].as_u64().unwrap_or(600));
    show(&user, &link);
    // The first look comes soon, because a person who was already signed in approves within a second or two.
    let mut wait = every.min(Duration::from_secs(1));
    while std::time::Instant::now() < until {
        tokio::time::sleep(wait).await;
        wait = every;
        let Ok(r) = http
            .post(format!("{base}/api/device/token"))
            .json(&json!({ "device_code": device }))
            .send()
            .await
        else {
            // A network blip while waiting is not the end: keep asking until the code runs out.
            continue;
        };
        match r.status().as_u16() {
            200 => {
                let t: Value = r.json().await.map_err(|e| e.to_string())?;
                return Ok(Config {
                    hub_url: ws_base(hub),
                    token: t["token"]
                        .as_str()
                        .ok_or("the hub sent no token")?
                        .to_string(),
                    // The name the person approved it under is the name the hub knows it by.
                    node_name: t["node"].as_str().unwrap_or(node).to_string(),
                });
            }
            202 => {}
            410 => return Err("the code was refused or ran out; run the command again".into()),
            other => return Err(format!("the hub answered {other} while waiting")),
        }
    }
    Err("nobody approved it in time; run the command again".into())
}
