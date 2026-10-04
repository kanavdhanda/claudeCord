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
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

/// Sends one message, but never waits for ever: a device that stopped reading fills its network buffer, and a plain send would then
/// stop this whole loop from checking its pings or its close order. A send ends when the actor says to drop the device, or when the
/// device has taken nothing for as long as silence is tolerated. Returns why it failed, if it did.
async fn send_bounded(
    socket: &mut WebSocket,
    msg: Message,
    kill: &tokio::sync::Notify,
    stall: Duration,
) -> Result<(), &'static str> {
    tokio::select! {
        r = socket.send(msg) => r.map_err(|_| "the connection broke while sending"),
        _ = kill.notified() => Err("its backlog went over the cap"),
        _ = tokio::time::sleep(stall) => Err("it stopped taking data"),
    }
}

/// Runs a device connection until it ends. Tells the actor when it starts and when it stops, and logs why it ended.
pub(crate) async fn run(
    mut socket: WebSocket,
    node: String,
    conn: u64,
    to_actor: mpsc::Sender<Input>,
    cfg: Config,
) {
    let (tx, mut rx) = mpsc::channel::<Out>(1024);
    let queued = Arc::new(AtomicUsize::new(0));
    let kill = Arc::new(tokio::sync::Notify::new());
    if to_actor
        .send(Input::Connected {
            node: node.clone(),
            conn,
            tx,
            queued: queued.clone(),
            kill: kill.clone(),
        })
        .await
        .is_err()
    {
        return;
    }
    crate::info!("hub", "{node} connected");
    let stall = cfg.ping_every * 2;
    let mut bucket = Bucket::new(600.0, 300.0, 0.0);
    let started = Instant::now();
    let mut last_seen = Instant::now();
    let mut ping = interval(cfg.ping_every);
    ping.tick().await;
    let mut close: Option<(u16, &'static str)> = None;
    // Why the connection ended, for the log.
    let mut why = "the device closed the connection".to_string();
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None => break,
                Some(Err(e)) => { why = format!("the connection failed: {e}"); break; }
                Some(Ok(msg)) => {
                    // Anything at all from the device, a ping answer included, shows it is alive.
                    last_seen = Instant::now();
                    match msg {
                        Message::Text(t) => {
                            if !bucket.take(1.0, started.elapsed().as_millis() as f64) {
                                why = "it sent frames faster than the limit".into();
                                close = Some((1008, "rate limit exceeded"));
                                break;
                            }
                            // Only well-formed, in-limit frames reach the core. Anything else is refused here.
                            if NodeFrame::parse(t.as_str()).is_none() {
                                crate::debug!("hub", "{node} sent a frame that was refused ({} bytes)", t.len());
                                let bad = serde_json::to_string(&HubFrame::Error { message: "bad frame".into() }).expect("plain data");
                                if let Err(e) = send_bounded(&mut socket, Message::Text(bad.into()), &kill, stall).await { why = e.into(); break; }
                                continue;
                            }
                            if to_actor.send(Input::Frame { node: node.clone(), text: t.as_str().to_string() }).await.is_err() { why = "the hub is stopping".into(); break; }
                        }
                        Message::Pong(_) => {
                            if to_actor.send(Input::Alive { node: node.clone() }).await.is_err() { why = "the hub is stopping".into(); break; }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            },
            out = rx.recv() => match out {
                None => { why = "the hub ended the connection".into(); close = Some((1013, "try again later")); break; }
                Some(Out::Close(code, reason)) => { why = format!("the hub closed it: {reason}"); close = Some((code, reason)); break; }
                Some(Out::Frame(text)) => {
                    let n = text.len();
                    let sent = send_bounded(&mut socket, Message::Text(text.into()), &kill, stall).await;
                    queued.fetch_sub(n.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
                    if let Err(e) = sent { why = e.into(); break; }
                }
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > stall {
                    why = format!("no sign of life for {:?}", last_seen.elapsed());
                    close = Some((1001, "no sign of life"));
                    break;
                }
                if let Err(e) = send_bounded(&mut socket, Message::Ping(Vec::new().into()), &kill, stall).await { why = e.into(); break; }
            }
        }
    }
    if let Some((code, reason)) = close {
        // Best effort, and bounded: a device that is not reading must not hold this task here.
        let _ = send_bounded(
            &mut socket,
            Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })),
            &kill,
            Duration::from_millis(500),
        )
        .await;
    }
    crate::info!(
        "hub",
        "{node} disconnected after {:?}: {why}",
        started.elapsed()
    );
    let _ = to_actor.send(Input::Disconnected { node, conn }).await;
}
