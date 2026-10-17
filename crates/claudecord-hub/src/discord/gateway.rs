//! The connection that carries Discord's live events (new messages, button presses, slash commands) to the bridge. It does
//! what Discord's protocol asks: say who the bot is, send a heartbeat on the interval Discord names, notice when Discord
//! stops answering, and reconnect, resuming where it left off when it can so no event is missed.

use super::api::Rest;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Events Discord should send: servers, messages in servers, and the text of those messages.
pub const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 10) | (1 << 15);

/// An event from Discord.
#[derive(Debug, Clone)]
pub struct Event {
    /// Its name, for example `MESSAGE_CREATE` or `INTERACTION_CREATE`.
    pub name: String,
    pub data: Value,
}

/// How to connect. `url` replaces the address Discord gives out (tests use it).
#[derive(Clone)]
pub struct GatewayOpts {
    pub token: String,
    pub url: Option<String>,
    /// Longest pause between reconnect attempts.
    pub backoff_max: Duration,
}

/// The name of the event the gateway makes up when its connection to Discord ends.
pub const DISCONNECTED: &str = "CC_DISCONNECTED";

/// Starts the connection in the background. Events arrive on `events`. It runs until the receiver is dropped.
pub fn spawn(
    rest: Rest,
    opts: GatewayOpts,
    events: mpsc::Sender<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(rest, opts, events))
}

/// What is remembered between connections so a reconnect can resume.
#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

/// The reconnect loop.
async fn run(rest: Rest, opts: GatewayOpts, events: mpsc::Sender<Event>) {
    let mut session = Session::default();
    let mut failures = 0u32;
    loop {
        let base = match (&session.resume_url, &opts.url) {
            (_, Some(u)) => u.clone(),
            (Some(u), None) => u.clone(),
            (None, None) => rest
                .gateway_url()
                .await
                .unwrap_or_else(|_| "wss://gateway.discord.gg".into()),
        };
        let url = with_query(&base);
        let started = tokio::time::Instant::now();
        let alive = connected(&url, &opts, &mut session, &events).await;
        if events.is_closed() {
            return;
        }
        // Not a Discord event: the bridge uses it to record that the connection is down (see `crate::uptime`).
        let _ = events
            .send(Event {
                name: DISCONNECTED.into(),
                data: Value::Null,
            })
            .await;
        failures = if started.elapsed() > Duration::from_secs(30) {
            1
        } else {
            failures.saturating_add(1)
        };
        let _ = alive;
        let cap = Duration::from_secs(1u64 << failures.min(5)).min(opts.backoff_max);
        let mut r = [0u8; 2];
        let _ = getrandom::fill(&mut r);
        tokio::time::sleep(cap.mul_f64(0.5 + u16::from_le_bytes(r) as f64 / 131_072.0)).await;
    }
}

/// Adds the protocol version and encoding to a gateway address, keeping whatever path it has (a bare host gets `/`).
fn with_query(base: &str) -> String {
    if base.contains('?') {
        return base.to_string();
    }
    let after_scheme = base.split("://").nth(1).unwrap_or(base);
    let root = if after_scheme.contains('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    format!("{root}?v=10&encoding=json")
}

/// One connection, from hello to whatever ends it. Returns false if it never got as far as being identified.
async fn connected(
    url: &str,
    opts: &GatewayOpts,
    session: &mut Session,
    events: &mpsc::Sender<Event>,
) -> bool {
    let Ok((ws, _)) = tokio_tungstenite::connect_async(url).await else {
        return false;
    };
    let (mut tx, mut rx) = ws.split();
    // Discord speaks first: hello, with how often to send a heartbeat.
    let interval = loop {
        match rx.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                if v["op"] == 10 {
                    break Duration::from_millis(
                        v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250),
                    );
                }
            }
            Some(Ok(_)) => continue,
            _ => return false,
        }
    };
    let hello = match (&session.id, session.seq) {
        (Some(id), Some(seq)) => {
            json!({"op": 6, "d": {"token": opts.token, "session_id": id, "seq": seq}})
        }
        _ => {
            json!({"op": 2, "d": {"token": opts.token, "intents": INTENTS, "properties": {"os": std::env::consts::OS, "browser": "claudecord", "device": "claudecord"}}})
        }
    };
    if tx
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return false;
    }
    let mut beat = tokio::time::interval(interval);
    beat.tick().await;
    let mut acked = true;
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { return true };
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { return true; }
                    continue;
                };
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                if let Some(s) = v["s"].as_u64() { session.seq = Some(s); }
                match v["op"].as_u64() {
                    Some(0) => {
                        let name = v["t"].as_str().unwrap_or("").to_string();
                        if name == "READY" {
                            session.id = v["d"]["session_id"].as_str().map(String::from);
                            session.resume_url = v["d"]["resume_gateway_url"].as_str().map(String::from);
                        }
                        if events.send(Event { name, data: v["d"].clone() }).await.is_err() { return true; }
                    }
                    Some(1) => { let _ = tx.send(Message::Text(json!({"op": 1, "d": session.seq}).to_string().into())).await; }
                    Some(7) => return true,
                    Some(9) => {
                        // Discord cannot resume this session. Start fresh next time.
                        if v["d"] != true { *session = Session::default(); }
                        return true;
                    }
                    Some(11) => acked = true,
                    _ => {}
                }
            }
            _ = beat.tick() => {
                // No answer to the last heartbeat means the connection is dead even if it looks open.
                if !acked { return true; }
                acked = false;
                if tx.send(Message::Text(json!({"op": 1, "d": session.seq}).to_string().into())).await.is_err() { return true; }
            }
        }
    }
}
