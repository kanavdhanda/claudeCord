//! The hub's disk writes, kept off the async runtime. The database syncs every commit to disk (so nothing acknowledged is lost
//! in a crash), and a sync can take tens of milliseconds on a slow disk. Done inside the actor's own task, that would stop every
//! connection's keepalive and message handling for as long as the disk takes, and on a one-core server it would stop all of them.
//! So writes are handed to a thread of their own, in order, and the actor carries on. Reads stay on the actor's own connection
//! (the database allows readers and one writer at once).
//!
//! What is saved is unchanged: history rows and audit lines are written in the order they were handed over, and of several
//! snapshots waiting at once only the newest is written, because it replaces the older ones anyway. `flush` waits until
//! everything handed over is on disk, and shutdown calls it. An in-memory database (used by some tests) cannot be shared with a
//! second connection, so it is written inline instead.

use crate::store::{HistoryRow, Store};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

enum Job {
    History(Vec<HistoryRow>),
    Snapshot(String, i64),
    Audit(i64, String, String, String),
    Flush(Sender<()>),
}

enum Mode {
    Thread {
        tx: Sender<Job>,
        join: Option<JoinHandle<()>>,
    },
    Inline,
}

/// The actor's handle on the database: reads directly, writes through the writer thread.
pub(crate) struct Disk {
    store: Store,
    mode: Mode,
}

impl Disk {
    pub(crate) fn new(store: Store) -> Self {
        let Some(mut writer) = store.fork() else {
            return Self {
                store,
                mode: Mode::Inline,
            };
        };
        let (tx, rx) = channel::<Job>();
        let join = std::thread::Builder::new()
            .name("hub-disk".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Take everything waiting, so a snapshot that was already replaced by a newer one is never written.
                    let mut jobs = vec![first];
                    jobs.extend(rx.try_iter());
                    let newest_snapshot = jobs.iter().rposition(|j| matches!(j, Job::Snapshot(..)));
                    for (i, job) in jobs.into_iter().enumerate() {
                        match job {
                            Job::History(rows) => {
                                if let Err(e) = writer.append(&rows) {
                                    crate::error!("hub", "could not save history: {e}");
                                }
                            }
                            Job::Snapshot(body, at) if Some(i) == newest_snapshot => {
                                if let Err(e) = writer.save_snapshot(&body, at) {
                                    crate::error!("hub", "could not save state: {e}");
                                }
                            }
                            Job::Snapshot(..) => {}
                            Job::Audit(at, project, who, what) => {
                                let _ = writer.audit(at, &project, &who, &what);
                            }
                            Job::Flush(done) => {
                                let _ = done.send(());
                            }
                        }
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

    pub(crate) fn append(&mut self, rows: Vec<HistoryRow>) {
        match &mut self.mode {
            Mode::Thread { tx, .. } => {
                let _ = tx.send(Job::History(rows));
            }
            Mode::Inline => {
                let _ = self.store.append(&rows);
            }
        }
    }

    pub(crate) fn snapshot(&mut self, body: String, at: i64) {
        match &mut self.mode {
            Mode::Thread { tx, .. } => {
                let _ = tx.send(Job::Snapshot(body, at));
            }
            Mode::Inline => {
                let _ = self.store.save_snapshot(&body, at);
            }
        }
    }

    pub(crate) fn audit(&mut self, at: i64, project: &str, who: &str, what: &str) {
        match &mut self.mode {
            Mode::Thread { tx, .. } => {
                let _ = tx.send(Job::Audit(at, project.into(), who.into(), what.into()));
            }
            Mode::Inline => {
                let _ = self.store.audit(at, project, who, what);
            }
        }
    }

    /// Waits until everything handed over so far is on disk.
    pub(crate) fn flush(&mut self) {
        if let Mode::Thread { tx, .. } = &self.mode {
            let (done, wait) = channel();
            if tx.send(Job::Flush(done)).is_ok() {
                let _ = wait.recv();
            }
        }
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
