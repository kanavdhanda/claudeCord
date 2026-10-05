//! The actor: the one task that owns the hub core and the store. Every input from every device, the chat bridge and the
//! timers arrives here through one queue, so the core never has to be locked and always sees events in a single order.
//! After each batch it saves what changed in one transaction and turns the core's effects into actions.

use super::disk::{Audit, Disk};
use super::{Config, Input, Out, Reply, now_ms};
use crate::hub::{Chat, Effect, HubCore, Persist};
use crate::protocol::{HubFrame, NodeFrame};
use crate::store::{EventRow, HistoryRow, Store};
use std::collections::{HashMap, VecDeque};
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

/// A batch whose effects and replies wait until its changes are on disk. `durable` is None while the write is in flight, then whether it
/// reached the disk.
struct Held {
    id: u64,
    durable: Option<bool>,
    fx: Vec<Effect>,
    replies: Vec<Reply>,
}

/// Loads the core's saved state: the saved rows, or, from a hub that still has the older single-text save, that (and then every row is
/// written again, so the next save moves it into rows).
fn load_core(core: &mut HubCore, store: &Store) {
    match store.load_state() {
        Ok(rows) if !rows.is_empty() => core.restore_rows(rows),
        _ => {
            if let Ok(Some(saved)) = store.load_snapshot()
                && core.restore(&saved)
            {
                core.mark_all_dirty();
            }
        }
    }
}

/// Pulls the history rows and audit lines out of a batch's effects: they are written with the batch, before anything else it caused.
fn split_persist(fx: &mut Vec<Effect>) -> (Vec<HistoryRow>, Vec<Audit>, Vec<EventRow>) {
    let (mut rows, mut audits, mut events, mut rest) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::with_capacity(fx.len()),
    );
    for e in fx.drain(..) {
        match e {
            Effect::Persist(Persist::History {
                project,
                thread,
                from,
                kind,
                text,
                at,
            }) => {
                rows.push(HistoryRow {
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
            }) => audits.push((at, project, who, what)),
            Effect::Persist(Persist::Event {
                project,
                kind,
                a,
                b,
                n,
                at,
            }) => events.push((at, project, kind.to_string(), a, b, n)),
            Effect::Persist(_) => {}
            other => rest.push(other),
        }
    }
    *fx = rest;
    (rows, audits, events)
}

/// Carries out, in order, every batch at the front of the queue whose changes are on disk: first what the core asked for (frames to
/// devices, chat posts), then the replies callers are waiting on. Nothing is sent or answered before its change is durable.
fn release(
    held: &mut VecDeque<Held>,
    conns: &mut HashMap<u64, Conn>,
    chat: &broadcast::Sender<Chat>,
    max_out: usize,
) {
    while held.front().is_some_and(|h| h.durable.is_some()) {
        let Some(h) = held.pop_front() else { break };
        if h.durable == Some(false) {
            crate::error!(
                "hub",
                "carrying on with a batch that could not be written to disk"
            );
        }
        // Doing what the core asked is guarded: a bug here must cost one batch of effects, never the actor and with it the hub.
        let carried = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            carry_out(h.fx, conns, chat, max_out)
        }));
        if carried.is_err() {
            crate::error!(
                "hub",
                "a panic while carrying out effects; that batch was dropped and the hub carries on"
            );
        }
        for reply in h.replies {
            reply();
        }
    }
}

/// Runs until told to shut down (or until nothing can send it work any more).
///
/// Every batch of inputs is handled against the core, then its history rows and the changes to the saved state go to the disk writer
/// as ONE transaction, and only when that is on disk are its frames sent, chat posts made and callers answered. So nothing is ever
/// acknowledged that a crash could lose, and many batches that arrive during one write share the next (group commit).
pub(crate) async fn run(
    mut core: HubCore,
    store: Store,
    mut inbox: mpsc::Receiver<Input>,
    chat: broadcast::Sender<Chat>,
    cfg: Config,
) {
    let mut disk = Disk::new(store);
    load_core(&mut core, disk.reader());
    let mut conns: HashMap<u64, Conn> = HashMap::new();
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
    let mut held: VecDeque<Held> = VecDeque::new();
    let (written_tx, mut written_rx) = mpsc::unbounded_channel::<(u64, bool)>();
    let mut next_id = 0u64;
    let mut stopping = None;
    let mut closed = false;
    loop {
        let now = now_ms();
        let mut fx: Vec<Effect> = Vec::new();
        // Notices about failures that were caught while handling this batch, and the replies callers wait for.
        let mut report: Vec<Effect> = Vec::new();
        let mut replies: Vec<Reply> = Vec::new();
        tokio::select! {
            input = inbox.recv(), if !closed => {
                let Some(input) = input else { closed = true; continue };
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
                            // The welcome acknowledges nothing and promises nothing, so it is not held for the disk: a slow write must not make
                            // a machine think the hub did not answer.
                            carry_out(vec![Effect::Send { conn, frame: HubFrame::Welcome { node_id: node.clone() } }], &mut conns, &chat, cfg.max_out_bytes);
                            fx.extend(guard(&mut core, disk.reader(), &conns, &mut report, Some(&node), |c| c.node_connected(&node, conn)).unwrap_or_default());
                        }
                        Input::Disconnected { node, conn } => {
                            conns.remove(&conn);
                            fx.extend(guard(&mut core, disk.reader(), &conns, &mut report, Some(&node), |c| c.node_disconnected(&node, conn)).unwrap_or_default());
                        }
                        Input::Alive { node } => core.touch(&node, now),
                        Input::Frame { node, text, stamp } => {
                            if let Some(frame) = NodeFrame::parse(&text) {
                                // A frame the machine sent again (it had not seen the ack) is taken once. It is acknowledged either way,
                                // and the ack is held with the rest of the batch until the change it caused is on disk.
                                let fresh = stamp.is_none_or(|(e, n)| core.accept_seq(&node, e, n));
                                if fresh {
                                    fx.extend(guard(&mut core, disk.reader(), &conns, &mut report, Some(&node), |c| c.on_node_frame(&node, frame, now)).unwrap_or_default());
                                }
                                if let Some((_, n)) = stamp && let Some(conn) = core.conn_of(&node) {
                                    fx.push(Effect::Send { conn, frame: HubFrame::Ack { n } });
                                }
                            }
                        }
                        Input::Call(f) => {
                            if let Some((e, reply)) = guard(&mut core, disk.reader(), &conns, &mut report, None, |c| f(c, now)) {
                                fx.extend(e);
                                replies.push(reply);
                            }
                        }
                        Input::Read(f) => {
                            // Reading changes nothing, so it is answered at once; a bug in the reader costs only that answer.
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&core, now))).is_err() {
                                crate::error!("hub", "a read of the core panicked; that answer was dropped");
                            }
                        }
                        Input::Shutdown { done: d } => stopping = Some(d),
                    }
                }
            }
            _ = tick.tick() => { fx.extend(guard(&mut core, disk.reader(), &conns, &mut report, None, |c| c.tick(now)).unwrap_or_default()); }
            _ = rollover.tick() => {
                if rolling.as_ref().is_none_or(|h| h.is_finished()) && let Some(mut s) = disk.reader().fork() {
                    // With the usual window of days, only whole days move out of the database, so each project gets one file per day and
                    // not one per hour (a window shorter than a day is only used to try this out in small tests); joining any small
                    // files a day ends up with follows.
                    let cutoff = now - cfg.hot_window.as_millis() as i64;
                    let cutoff = if cfg.hot_window >= Duration::from_secs(86_400) { cutoff - cutoff.rem_euclid(86_400_000) } else { cutoff };
                    let events_before = now - 90 * 86_400_000;
                    rolling = Some(tokio::task::spawn_blocking(move || {
                        // The graphs look back at most a quarter of a year, so older events only take up room.
                        if let Err(e) = s.prune_events(events_before) {
                            crate::error!("hub", "forgetting old events failed: {e}");
                        }
                        match s.rollover(cutoff) {
                            Ok(n) if n > 0 => crate::info!("hub", "moved {n} old history rows out of the database into files"),
                            Ok(_) => {}
                            Err(e) => crate::error!("hub", "moving old history out of the database failed: {e}"),
                        }
                        if let Err(e) = s.compact(100_000) {
                            crate::error!("hub", "joining old history files failed: {e}");
                        }
                    }));
                }
            }
            _ = backup.tick() => {
                if backing_up.as_ref().is_none_or(|h| h.is_finished()) && let Some(s) = disk.reader().fork() {
                    backing_up = Some(tokio::task::spawn_blocking(move || {
                        if let Err(e) = s.backup_to_bucket(now_ms() / 1000) { crate::error!("hub", "backup to the bucket failed: {e}"); }
                    }));
                }
            }
            Some((id, ok)) = written_rx.recv() => {
                if let Some(h) = held.iter_mut().find(|h| h.id == id) {
                    h.durable = Some(ok);
                }
            }
        }
        fx.extend(report);
        // The batch's history and the changes it made to the saved state go to disk together, and what it caused waits for that.
        let (rows, audits, events) = split_persist(&mut fx);
        let changes = core.take_changes();
        let needs_write =
            !rows.is_empty() || !audits.is_empty() || !events.is_empty() || !changes.is_empty();
        if needs_write || !fx.is_empty() || !replies.is_empty() {
            let id = next_id;
            next_id += 1;
            let durable = if needs_write {
                let wait = disk.commit(rows, audits, events, vec![changes]);
                let told = written_tx.clone();
                tokio::spawn(async move {
                    let _ = told.send((id, wait.await.unwrap_or(false)));
                });
                None
            } else {
                Some(true)
            };
            held.push_back(Held {
                id,
                durable,
                fx,
                replies,
            });
        }
        release(&mut held, &mut conns, &chat, cfg.max_out_bytes);
        // Stopping waits until everything handed over is on disk and has been carried out.
        if (stopping.is_some() || closed) && held.is_empty() {
            break;
        }
    }
    // Shutting down: tell every device, so each reconnects at once instead of waiting to notice. Everything is already on disk.
    for c in conns.values() {
        let _ = c.tx.try_send(Out::Close(1001, "hub is restarting"));
    }
    if let Some(d) = stopping {
        let _ = d.send(());
    }
}

/// Runs one step against the core and survives a bug in it. If the step panics, that one input is dropped, the core is
/// rebuilt from the last saved state (plus the connections that are open right now), and the hub carries on serving
/// everyone else. A bug in one handler must not leave every device talking to a dead hub. Returns None if it panicked.
///
/// A failure is reported before anything else: the projects it could have touched (those on `node`, or all of them when no machine
/// is involved) are told in their chat, with the owner pinged, that a request was dropped and what that may mean. `report` collects
/// those notices for the caller to carry out with the rest of the batch.
fn guard<R>(
    core: &mut HubCore,
    store: &Store,
    conns: &HashMap<u64, Conn>,
    report: &mut Vec<Effect>,
    node: Option<&str>,
    step: impl FnOnce(&mut HubCore) -> R,
) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(core))) {
        Ok(r) => Some(r),
        Err(_) => {
            // Which projects to tell is worked out before the core is rebuilt, while it still knows who was where.
            let projects = match node {
                Some(n) => core.projects_of_node(n),
                None => core.projects(),
            };
            crate::error!(
                "hub",
                "a handler panicked; restoring the core from its last saved state"
            );
            *core = HubCore::default();
            load_core(core, store);
            for (conn, c) in conns {
                core.node_connected(&c.node, *conn);
                core.assume_online(&c.node);
            }
            for project in projects {
                report.push(Effect::Chat(Chat::Notice {
                    project,
                    text: "Internal error: the hub hit a bug while handling one request. It dropped that request and recovered from its last saved state, so anything from the last moments may need repeating. The details are in the hub's log.".into(),
                    mention: true,
                }));
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
            // History and audit lines were written with the batch (see `split_persist`).
            Effect::Persist(_) | Effect::AcceptCheck { .. } => {}
        }
    }
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
