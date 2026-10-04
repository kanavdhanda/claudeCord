//! The actor: the one task that owns the hub core and the store. Every input from every device, the chat bridge and the
//! timers arrives here through one queue, so the core never has to be locked and always sees events in a single order.
//! After each batch it saves what changed in one transaction and turns the core's effects into actions.

use super::disk::Disk;
use super::{Config, Input, Out, now_ms};
use crate::hub::{Chat, Effect, HubCore, Persist};
use crate::protocol::{HubFrame, NodeFrame};
use crate::store::{HistoryRow, Store};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// A connected device, as the actor sees it.
struct Conn {
    node: String,
    tx: mpsc::Sender<Out>,
    queued: Arc<AtomicUsize>,
    kill: Arc<tokio::sync::Notify>,
}

/// Runs until told to shut down (or until nothing can send it work any more).
pub(crate) async fn run(
    mut core: HubCore,
    store: Store,
    mut inbox: mpsc::Receiver<Input>,
    chat: broadcast::Sender<Chat>,
    cfg: Config,
) {
    let mut disk = Disk::new(store);
    if let Ok(Some(saved)) = disk.reader().load_snapshot() {
        core.restore(&saved);
    }
    let mut conns: HashMap<u64, Conn> = HashMap::new();
    let mut history: Vec<HistoryRow> = Vec::new();
    let mut dirty = false;
    let mut save = tokio::time::interval(cfg.save_every);
    let mut tick = tokio::time::interval(cfg.tick_every);
    // History rollover runs on its own timer, in the background, on a second database connection.
    let mut rollover = tokio::time::interval(
        cfg.rollover_every
            .unwrap_or(Duration::from_secs(100 * 365 * 24 * 3600)),
    );
    rollover.tick().await;
    let mut rolling: Option<tokio::task::JoinHandle<()>> = None;
    // The live database is copied to the bucket on its own timer too, again in the background.
    let mut backup = tokio::time::interval(
        cfg.backup_every
            .unwrap_or(Duration::from_secs(100 * 365 * 24 * 3600)),
    );
    backup.tick().await;
    let mut backing_up: Option<tokio::task::JoinHandle<()>> = None;
    let mut done = None;
    loop {
        let now = now_ms();
        let mut fx: Vec<Effect> = Vec::new();
        tokio::select! {
            input = inbox.recv() => {
                let Some(input) = input else { break };
                // Take everything already waiting, so a burst is handled as one batch and saved with one write.
                let mut batch = vec![input];
                while batch.len() < 256 && let Ok(more) = inbox.try_recv() {
                    batch.push(more);
                }
                for input in batch {
                    match input {
                        Input::Auth { token, reply } => {
                            let _ = reply.send(disk.reader().node_for_token(&token).ok().flatten());
                        }
                        Input::Connected { node, conn, tx, queued, kill } => {
                            core.touch(&node, now);
                            conns.insert(conn, Conn { node: node.clone(), tx, queued, kill });
                            fx.push(Effect::Send { conn, frame: HubFrame::Welcome { node_id: node.clone() } });
                            fx.extend(guard(&mut core, disk.reader(), &conns, |c| c.node_connected(&node, conn)).unwrap_or_default());
                            dirty = true;
                        }
                        Input::Disconnected { node, conn } => {
                            conns.remove(&conn);
                            fx.extend(guard(&mut core, disk.reader(), &conns, |c| c.node_disconnected(&node, conn)).unwrap_or_default());
                            dirty = true;
                        }
                        Input::Alive { node } => core.touch(&node, now),
                        Input::Frame { node, text } => {
                            if let Some(frame) = NodeFrame::parse(&text) {
                                fx.extend(guard(&mut core, disk.reader(), &conns, |c| c.on_node_frame(&node, frame, now)).unwrap_or_default());
                                dirty = true;
                            }
                        }
                        Input::Call(f) => {
                            fx.extend(guard(&mut core, disk.reader(), &conns, |c| f(c, now)).unwrap_or_default());
                            dirty = true;
                        }
                        Input::Shutdown { done: d } => done = Some(d),
                    }
                }
            }
            _ = tick.tick() => { fx.extend(guard(&mut core, disk.reader(), &conns, |c| c.tick(now)).unwrap_or_default()); dirty = true; }
            _ = rollover.tick() => {
                if rolling.as_ref().is_none_or(|h| h.is_finished()) && let Some(mut s) = disk.reader().fork() {
                    let cutoff = now - cfg.hot_window.as_millis() as i64;
                    rolling = Some(tokio::task::spawn_blocking(move || { let _ = s.rollover(cutoff); }));
                }
            }
            _ = backup.tick() => {
                if backing_up.as_ref().is_none_or(|h| h.is_finished()) && let Some(s) = disk.reader().fork() {
                    backing_up = Some(tokio::task::spawn_blocking(move || {
                        if let Err(e) = s.backup_to_bucket(now_ms() / 1000) { eprintln!("hub: backup to the bucket failed: {e}"); }
                    }));
                }
            }
            _ = save.tick() => {
                if dirty {
                    disk.snapshot(core.snapshot(), now);
                    dirty = false;
                }
            }
        }
        carry_out(
            fx,
            &mut conns,
            &chat,
            &mut history,
            &mut disk,
            now,
            cfg.max_out_bytes,
        );
        if !history.is_empty() {
            disk.append(std::mem::take(&mut history));
        }
        if done.is_some() {
            break;
        }
    }
    // Shutting down: tell every device, so each reconnects at once instead of waiting to notice, then save.
    for c in conns.values() {
        let _ = c.tx.try_send(Out::Close(1001, "hub is restarting"));
    }
    disk.snapshot(core.snapshot(), now_ms());
    // Everything handed over is on disk before the hub says it has stopped.
    disk.flush();
    if let Some(d) = done {
        let _ = d.send(());
    }
}

/// Runs one step against the core and survives a bug in it. If the step panics, that one input is dropped, the core is
/// rebuilt from the last saved state (plus the connections that are open right now), and the hub carries on serving
/// everyone else. A bug in one handler must not leave every device talking to a dead hub. Returns None if it panicked.
fn guard<R>(
    core: &mut HubCore,
    store: &Store,
    conns: &HashMap<u64, Conn>,
    step: impl FnOnce(&mut HubCore) -> R,
) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(core))) {
        Ok(r) => Some(r),
        Err(_) => {
            eprintln!("hub: a handler panicked; restoring the core from its last saved state");
            *core = HubCore::default();
            if let Ok(Some(saved)) = store.load_snapshot() {
                core.restore(&saved);
            }
            for (conn, c) in conns {
                core.node_connected(&c.node, *conn);
                core.assume_online(&c.node);
            }
            None
        }
    }
}

/// Does what the core asked for: send frames, close connections, show things in chat, and note what to store. A device
/// whose backlog is too large is dropped here, because a device that does not read must not make the hub hold its mail.
fn carry_out(
    fx: Vec<Effect>,
    conns: &mut HashMap<u64, Conn>,
    chat: &broadcast::Sender<Chat>,
    history: &mut Vec<HistoryRow>,
    disk: &mut Disk,
    now: i64,
    max_out: usize,
) {
    for e in fx {
        match e {
            Effect::Send { conn, frame } => send(conns, conn, &frame, max_out),
            Effect::Close { conn } => {
                if let Some(c) = conns.remove(&conn) {
                    let _ = c.tx.try_send(Out::Close(1000, "replaced"));
                }
            }
            Effect::Chat(c) => {
                // With nobody listening (no chat bridge yet) this is simply dropped.
                let _ = chat.send(c);
            }
            Effect::Persist(Persist::History {
                project,
                thread,
                from,
                kind,
                text,
                at,
            }) => {
                history.push(HistoryRow {
                    id: 0,
                    at,
                    project,
                    thread,
                    from,
                    kind: kind.to_string(),
                    text,
                });
            }
            Effect::Persist(Persist::Audit {
                project,
                who,
                what,
                at,
            }) => {
                disk.audit(at, &project, &who, &what);
            }
            Effect::Persist(_) | Effect::AcceptCheck { .. } => {}
        }
    }
    let _ = now;
}

/// Queues one frame for a device. If the device's backlog or queue is full, it is dropped (and will reconnect).
fn send(conns: &mut HashMap<u64, Conn>, conn: u64, frame: &HubFrame, max_out: usize) {
    let Some(c) = conns.get(&conn) else { return };
    let text = serde_json::to_string(frame).expect("plain data");
    let n = text.len();
    if c.queued.load(Ordering::Relaxed) + n > max_out {
        // The close order would queue behind the very backlog that is stuck, so the session is also told directly.
        let _ = c.tx.try_send(Out::Close(1013, "not reading fast enough"));
        c.kill.notify_one();
        conns.remove(&conn);
        return;
    }
    c.queued.fetch_add(n, Ordering::Relaxed);
    if c.tx.try_send(Out::Frame(text)).is_err() {
        conns.remove(&conn);
    }
}
