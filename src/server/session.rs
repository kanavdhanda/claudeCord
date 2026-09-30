//! One device's WebSocket, from upgrade to close. It enforces the rules that keep one bad or broken device from hurting
//! the hub: it pings on a timer and drops a device that goes silent, caps how fast and how large messages may be,
//! parses and validates every frame before the core sees it, and drops a device that stops reading instead of storing
//! its backlog without limit.

use super::{Config, Input, Out};
use crate::protocol::{HubFrame, NodeFrame};
use crate::security::limits::Bucket;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

/// Runs a device connection until it ends. Tells the actor when it starts and when it stops.
pub(crate) async fn run(
    mut socket: WebSocket,
    node: String,
    conn: u64,
    to_actor: mpsc::Sender<Input>,
    cfg: Config,
) {
    let (tx, mut rx) = mpsc::channel::<Out>(1024);
    let queued = Arc::new(AtomicUsize::new(0));
    if to_actor
        .send(Input::Connected {
            node: node.clone(),
            conn,
            tx,
            queued: queued.clone(),
        })
        .await
        .is_err()
    {
        return;
    }
    let mut bucket = Bucket::new(600.0, 300.0, 0.0);
    let started = Instant::now();
    let mut last_seen = Instant::now();
    let mut ping = interval(cfg.ping_every);
    ping.tick().await;
    let mut close: Option<(u16, &'static str)> = None;
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(msg)) => {
                    // Anything at all from the device, a ping answer included, shows it is alive.
                    last_seen = Instant::now();
                    match msg {
                        Message::Text(t) => {
                            if !bucket.take(1.0, started.elapsed().as_millis() as f64) {
                                close = Some((1008, "rate limit exceeded"));
                                break;
                            }
                            // Only well-formed, in-limit frames reach the core. Anything else is refused here.
                            if NodeFrame::parse(t.as_str()).is_none() {
                                let bad = serde_json::to_string(&HubFrame::Error { message: "bad frame".into() }).expect("plain data");
                                if socket.send(Message::Text(bad.into())).await.is_err() { break; }
                                continue;
                            }
                            if to_actor.send(Input::Frame { node: node.clone(), text: t.as_str().to_string() }).await.is_err() { break; }
                        }
                        Message::Pong(_) => {
                            if to_actor.send(Input::Alive { node: node.clone() }).await.is_err() { break; }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            },
            out = rx.recv() => match out {
                None => { close = Some((1013, "try again later")); break; }
                Some(Out::Close(code, why)) => { close = Some((code, why)); break; }
                Some(Out::Frame(text)) => {
                    let n = text.len();
                    let sent = socket.send(Message::Text(text.into())).await;
                    queued.fetch_sub(n.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
                    if sent.is_err() { break; }
                }
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > cfg.ping_every * 2 { close = Some((1001, "no sign of life")); break; }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
        }
    }
    if let Some((code, why)) = close {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: why.into(),
            })))
            .await;
    }
    let _ = to_actor.send(Input::Disconnected { node, conn }).await;
}
