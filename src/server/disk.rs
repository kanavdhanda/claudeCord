//! The hub's disk writes, kept off the async runtime, and the one rule they serve: NOTHING is sent, reacted to or acknowledged until
//! the change that caused it is on disk. The actor hands each batch's history rows, audit lines and (when state changed) the core's
//! state changes to the writer thread as one commit, and holds the batch's effects until the commit is done. Many batches that arrive while
//! one is being written share the next write (group commit), so what a commit costs does not grow with how many messages there are.
//!
//! The database syncs every transaction to disk (WAL, synchronous FULL), and a commit is ONE transaction holding the history rows,
//! the audit lines and the changes to the saved state (only the rows that changed, see `crate::hub::tracked`), so after a crash the
//! state and the history always agree and nothing acknowledged is missing.
//!
//! Reads stay on the actor's own connection (the database allows readers and one writer at once). An in-memory database (used by some
//! tests) cannot be shared with a second connection, so it is written inline instead.

use crate::store::{Changes, EventRow, HistoryRow, Store};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

/// An audit line: (at, project, who, what).
pub(crate) type Audit = (i64, String, String, String);

struct Commit {
    rows: Vec<HistoryRow>,
    audits: Vec<Audit>,
    events: Vec<EventRow>,
    changes: Vec<Changes>,
    /// Told whether the commit reached the disk.
    done: oneshot::Sender<bool>,
}

enum Mode {
    Thread {
        tx: Sender<Commit>,
        join: Option<JoinHandle<()>>,
    },
    Inline,
}

/// The actor's handle on the database: reads directly, writes through the writer thread.
pub(crate) struct Disk {
    store: Store,
    mode: Mode,
}

/// Writes what is waiting in one transaction, trying a few times if the disk objects. Returns whether it is on disk.
fn write(
    store: &mut Store,
    rows: &[HistoryRow],
    audits: &[Audit],
    events: &[EventRow],
    changes: &[Changes],
) -> bool {
    for attempt in 0..4u32 {
        match store.commit(rows, audits, events, changes) {
            Ok(()) => return true,
            Err(e) => {
                crate::error!(
                    "hub",
                    "could not write to the database (try {}): {e}",
                    attempt + 1
                );
                std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
            }
        }
    }
    false
}

impl Disk {
    pub(crate) fn new(store: Store) -> Self {
        let Some(mut writer) = store.fork() else {
            return Self {
                store,
                mode: Mode::Inline,
            };
        };
        // Fault injection for the crash test: wait this long before each write, so a kill is likely to land between a change being made and
        // it being saved. Never set in production.
        let delay = std::env::var("CLAUDECORD_TEST_COMMIT_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(std::time::Duration::from_millis);
        let (tx, rx) = channel::<Commit>();
        // What a write gave up on stays here and goes out first with the next one, so a disk that was full or read-only for a while does not
        // cost what the machines were already told was saved (they will not send it again). Only this much is kept, so an outage that never
        // ends cannot fill the memory.
        const KEEP_ROWS: usize = 100_000;
        let join = std::thread::Builder::new()
            .name("hub-disk".into())
            .spawn(move || {
                let mut kept = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                while let Ok(first) = rx.recv() {
                    // Everything waiting becomes one transaction, with rows, audit lines and state changes in the order handed over.
                    let mut batch = vec![first];
                    batch.extend(rx.try_iter());
                    let (mut rows, mut audits, mut events, mut changes) = std::mem::take(&mut kept);
                    let mut waiting = Vec::new();
                    for c in batch {
                        rows.extend(c.rows);
                        audits.extend(c.audits);
                        events.extend(c.events);
                        changes.extend(c.changes);
                        waiting.push(c.done);
                    }
                    if let Some(d) = delay {
                        std::thread::sleep(d);
                    }
                    let ok = write(&mut writer, &rows, &audits, &events, &changes);
                    if !ok {
                        crate::error!("hub", "giving up on this write: the hub is running WITHOUT durability until the disk recovers");
                        if rows.len() + events.len() + audits.len() <= KEEP_ROWS {
                            kept = (rows, audits, events, changes);
                        } else {
                            crate::error!("hub", "the disk has been failing for so long that what is waiting for it is being dropped");
                        }
                    }
                    for done in waiting {
                        let _ = done.send(ok);
                    }
                }
            })
            .expect("the system can start a thread");
        Self {
            store,
            mode: Mode::Thread {
                tx,
                join: Some(join),
            },
        }
    }

    /// The actor's connection, for reading (tokens, the last saved state) and for making further connections.
    pub(crate) fn reader(&self) -> &Store {
        &self.store
    }

    /// Hands over one batch's changes. The receiver completes with whether they are on disk.
    pub(crate) fn commit(
        &mut self,
        rows: Vec<HistoryRow>,
        audits: Vec<Audit>,
        events: Vec<EventRow>,
        changes: Vec<Changes>,
    ) -> oneshot::Receiver<bool> {
        let (done, wait) = oneshot::channel();
        match &mut self.mode {
            Mode::Thread { tx, .. } => {
                if let Err(e) = tx.send(Commit {
                    rows,
                    audits,
                    events,
                    changes,
                    done,
                }) {
                    // The writer thread is gone; say so, and answer "not saved" so the actor does not wait for ever.
                    crate::error!(
                        "hub",
                        "the disk writer has stopped; changes are not being saved"
                    );
                    let _ = e.0.done.send(false);
                }
            }
            Mode::Inline => {
                let ok = write(&mut self.store, &rows, &audits, &events, &changes);
                let _ = done.send(ok);
            }
        }
        wait
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        if let Mode::Thread { tx, join } = &mut self.mode {
            // Closing the channel ends the thread once it has written what is waiting.
            let (closed, _) = channel();
            *tx = closed;
            if let Some(j) = join.take() {
                let _ = j.join();
            }
        }
    }
}
