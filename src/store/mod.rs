//! Durable storage for one tenant: a single SQLite file holding the core's saved state and the conversation history,
//! plus compressed files for history old enough to leave the database. Disk is the only thing that grows without bound
//! here, so history is built to shrink: recent days stay in the database where they can be searched, older days are
//! gzipped into one file per project per day and removed from the database, and the database stays small and fast.
//!
//! Every write is a transaction. The database runs in WAL mode with full syncing, so anything this module says it
//! saved is still there after a crash or a power cut.

pub mod bucket;
mod uptime;

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One line of conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRow {
    pub id: i64,
    pub at: i64,
    pub project: String,
    pub thread: Option<String>,
    pub from: String,
    pub kind: String,
    pub text: String,
}

/// One measurable thing that happened, as stored: (at, project, kind, a, b, n). See `crate::hub::Persist::Event` for what the kinds mean.
pub type EventRow = (i64, String, String, String, String, f64);

/// What to write to bring the saved state up to date: rows to insert or replace, rows to delete, and collections whose rows are all
/// replaced (their old rows are deleted first). Made by the hub core from what changed; see `crate::hub::tracked`.
#[derive(Default, Debug, Clone)]
pub struct Changes {
    pub upserts: Vec<(String, String)>,
    pub deletes: Vec<String>,
    /// Row-name prefixes (`name:`) whose rows are all deleted before the upserts.
    pub clears: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty() && self.clears.is_empty()
    }
}

/// A compressed file of old history and what it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub file: String,
    pub project: String,
    pub first_id: i64,
    pub last_id: i64,
    pub rows: i64,
    /// The UTC day (days since 1970) the rows are from, or -1 for a file made before days were recorded.
    pub day: i64,
}

const DAY_MS: i64 = 86_400_000;
/// The most rows in one history file. A day with more is written as several files, so rolling over never needs much memory.
const ROLLOVER_CHUNK: usize = 20_000;
/// How many unpacked history files are kept for paging back.
const SEGMENT_CACHE: usize = 8;

/// The name of a history file: project, UTC day, and the id range it holds, so no two files ever have the same name.
fn segment_name(project: &str, day: i64, first: i64, last: i64) -> String {
    format!(
        "{}-d{day}-{first}-{last}.jsonl.gz",
        crate::agents::text::safe_name(project)
    )
}

/// Rows as one compressed file of JSON lines, oldest first.
fn encode_segment(rows: &[HistoryRow]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for r in rows {
        writeln!(enc, "{}", serde_json::to_string(r).expect("plain data"))?;
    }
    enc.finish()
}

pub struct Store {
    conn: Connection,
    /// The database file, if it lives on disk.
    path: Option<PathBuf>,
    /// Folder for compressed history. None keeps everything in the database (tests, tiny installs).
    segments_dir: Option<PathBuf>,
    /// Where old history files go instead of staying on this machine's disk, if a bucket is set up.
    bucket: Option<bucket::Bucket>,
    /// The last few old history files read, decoded, so paging back through the same day does not unpack it again for every page.
    cache: std::sync::Mutex<std::collections::VecDeque<(String, std::sync::Arc<Vec<HistoryRow>>)>>,
}

impl Store {
    /// Opens (creating if needed) the database at `path`. Old history files go under `segments_dir` when given.
    pub fn open(path: &Path, segments_dir: Option<&Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let mut store = Self::init(conn, segments_dir)?;
        store.path = Some(path.to_path_buf());
        Ok(store)
    }

    /// An in-memory store, for tests.
    pub fn open_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    /// Sets the database up: durable settings and the tables, if they are not there yet.
    fn init(conn: Connection, segments_dir: Option<&Path>) -> rusqlite::Result<Self> {
        // The wait for a lock comes FIRST: switching to WAL itself needs a lock, and two connections opening the same file at once
        // (the hub opens several) would otherwise fail on the very first statement instead of waiting their turn.
        // Thirty seconds: a write queued behind a slow disk sync is not an error, and a lock held this long really is stuck.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snapshot (id INTEGER PRIMARY KEY CHECK (id = 1), at INTEGER NOT NULL, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS history (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, project TEXT NOT NULL,
                 thread TEXT, sender TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL CHECK (length(body) <= 100000));
             CREATE INDEX IF NOT EXISTS history_thread ON history (project, thread, id);
             CREATE INDEX IF NOT EXISTS history_time ON history (at);
             CREATE TABLE IF NOT EXISTS audit (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, project TEXT NOT NULL, who TEXT NOT NULL, what TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tokens (hash TEXT PRIMARY KEY, node TEXT NOT NULL, at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS tokens_node ON tokens (node);
             CREATE TABLE IF NOT EXISTS events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, project TEXT NOT NULL, kind TEXT NOT NULL,
                 a TEXT NOT NULL, b TEXT NOT NULL, n REAL NOT NULL);
             CREATE INDEX IF NOT EXISTS events_at ON events (at);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS uptime (component TEXT NOT NULL, at INTEGER NOT NULL, state TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS uptime_component ON uptime (component, at);
             CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, body TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS segments (
                 file TEXT PRIMARY KEY, project TEXT NOT NULL, first_id INTEGER NOT NULL, last_id INTEGER NOT NULL, rows INTEGER NOT NULL);",
        )?;
        // A database made before history files were grouped by day gets the column, with -1 meaning "not known".
        let has_day = conn
            .prepare("SELECT 1 FROM pragma_table_info('segments') WHERE name = 'day'")?
            .exists([])?;
        if !has_day {
            conn.execute(
                "ALTER TABLE segments ADD COLUMN day INTEGER NOT NULL DEFAULT -1",
                [],
            )?;
        }
        Ok(Self {
            conn,
            path: None,
            segments_dir: segments_dir.map(Path::to_path_buf),
            bucket: None,
            cache: Default::default(),
        })
    }

    /// Checks the database file is not damaged (SQLite's quick check). Ok when sound, else what it found.
    pub fn integrity(&self) -> Result<(), String> {
        let found: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if found == "ok" { Ok(()) } else { Err(found) }
    }

    /// A second connection to the same database file, for background work (like moving old history out) so the main
    /// connection is never held up by it. None for an in-memory store.
    pub fn fork(&self) -> Option<Store> {
        let path = self.path.as_ref()?;
        let mut s = Store::open(path, self.segments_dir.as_deref()).ok()?;
        s.bucket = self.bucket.clone();
        Some(s)
    }

    /// The bucket in use, if one is set.
    pub fn bucket_ref(&self) -> Option<&bucket::Bucket> {
        self.bucket.as_ref()
    }

    /// Sets (or clears) the bucket where old history files are kept. With one set, a rolled-over file is uploaded and then
    /// removed from local disk, and reading it later fetches it back.
    pub fn set_bucket(&mut self, b: Option<bucket::Bucket>) {
        self.bucket = b;
    }

    /// Remembers a small fact under a key (which chat channel belongs to which project, and the like). Replaces any earlier value.
    pub fn kv_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// A remembered fact, if there is one.
    pub fn kv_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
    }

    /// Every remembered fact whose key starts with `prefix`, as (key, value).
    pub fn kv_scan(&self, prefix: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key")?;
        let upper = format!("{prefix}\u{10FFFF}");
        let rows = st.query_map(params![prefix, upper], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Copies the whole live database (history, saved state, tokens) to the bucket, compressed, so a host that loses its disk
    /// loses nothing. It keeps the newest copy under `backup/hub-latest.db.gz` and one of seven rotating daily copies
    /// (`backup/hub-0.db.gz` ... `-6`), so a bad copy can be stepped back from. Returns the key written.
    pub fn backup_to_bucket(&self, now_secs: i64) -> std::io::Result<String> {
        let b = self
            .bucket
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no bucket is set up"))?;
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| std::io::Error::other("an in-memory database cannot be backed up"))?;
        let tmp = path.with_extension("backup.tmp");
        let _ = std::fs::remove_file(&tmp);
        // VACUUM INTO makes a consistent, compact copy while the database stays in use.
        self.conn
            .execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])
            .map_err(io)?;
        let raw = std::fs::read(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw)?;
        let bytes = enc.finish()?;
        let day = format!("backup/hub-{}.db.gz", (now_secs / 86_400) % 7);
        b.put(&day, &bytes)?;
        b.put("backup/hub-latest.db.gz", &bytes)?;
        Ok(day)
    }

    /// Brings a backup back from the bucket into a new database file. It will not replace a database that already has
    /// content unless `force` is set, because that would throw away newer history.
    pub fn restore_from_bucket(
        b: &bucket::Bucket,
        key: &str,
        dest: &Path,
        force: bool,
    ) -> std::io::Result<()> {
        if !force && std::fs::metadata(dest).is_ok_and(|m| m.len() > 0) {
            return Err(std::io::Error::other(format!(
                "{} already exists; pass --force to replace it",
                dest.display()
            )));
        }
        let mut raw = Vec::new();
        GzDecoder::new(&b.get(key)?[..]).read_to_end(&mut raw)?;
        // Check it really is a database before putting it in place.
        let tmp = dest.with_extension("restore.tmp");
        std::fs::write(&tmp, &raw)?;
        Connection::open(&tmp)
            .and_then(|c| {
                c.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| {
                    r.get::<_, i64>(0)
                })
            })
            .map_err(io)?;
        for ext in ["db-wal", "db-shm"] {
            let _ = std::fs::remove_file(dest.with_extension(ext));
        }
        std::fs::rename(&tmp, dest)
    }

    /// Every project that has any history, in the database or in compressed files.
    pub fn projects(&self) -> rusqlite::Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT project FROM history UNION SELECT project FROM segments ORDER BY 1")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// All of a project's history, oldest first: the compressed files first, then what is still in the database. A row
    /// present in both (after a crash in the middle of a rollover) appears once.
    pub fn history_all(&self, project: &str) -> std::io::Result<Vec<HistoryRow>> {
        let mut all: Vec<HistoryRow> = Vec::new();
        for seg in self.segments(project).map_err(io)? {
            all.extend(self.read_segment(&seg.file)?);
        }
        all.extend(
            self.history(project, None, 0, usize::MAX >> 1)
                .map_err(io)?,
        );
        all.sort_by_key(|r| r.id);
        all.dedup_by_key(|r| r.id);
        Ok(all)
    }

    /// Saves a batch's history rows, audit lines and changes to the core's state in ONE transaction: all of it is on disk, or none of it,
    /// so the state and the history can never disagree after a crash. `audits` are (at, project, who, what). Each entry of `changes`
    /// is applied in order, so a later change to a row wins.
    pub fn commit(
        &mut self,
        rows: &[HistoryRow],
        audits: &[(i64, String, String, String)],
        events: &[EventRow],
        changes: &[Changes],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut ev = tx.prepare_cached(
                "INSERT INTO events (at, project, kind, a, b, n) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (at, project, kind, a, b, n) in events {
                ev.execute(params![at, project, kind, a, b, n])?;
            }
            let mut st = tx.prepare_cached("INSERT INTO history (at, project, thread, sender, kind, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for r in rows {
                st.execute(params![r.at, r.project, r.thread, r.from, r.kind, r.text])?;
            }
            let mut au = tx.prepare_cached(
                "INSERT INTO audit (at, project, who, what) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (at, project, who, what) in audits {
                au.execute(params![at, project, who, what])?;
            }
            let mut up = tx.prepare_cached("INSERT INTO state (key, body) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET body = ?2")?;
            let mut del = tx.prepare_cached("DELETE FROM state WHERE key = ?1")?;
            for c in changes {
                for prefix in &c.clears {
                    // A prefix is `name:`; `substr` compares exactly, with no wildcard characters to escape.
                    tx.execute(
                        "DELETE FROM state WHERE substr(key, 1, ?2) = ?1",
                        params![prefix, prefix.len() as i64],
                    )?;
                }
                for (key, body) in &c.upserts {
                    up.execute(params![key, body])?;
                }
                for key in &c.deletes {
                    del.execute(params![key])?;
                }
            }
        }
        tx.commit()
    }

    /// The events of one project (or every project when `project` is None) from `since` on, oldest first.
    pub fn events_since(
        &self,
        project: Option<&str>,
        since: i64,
    ) -> rusqlite::Result<Vec<EventRow>> {
        let mut st = self.conn.prepare(
            "SELECT at, project, kind, a, b, n FROM events WHERE at >= ?1 AND (?2 IS NULL OR project = ?2) ORDER BY id",
        )?;
        let rows = st.query_map(params![since, project], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        rows.collect()
    }

    /// Forgets events older than `before`, so the table never grows without bound.
    pub fn prune_events(&mut self, before: i64) -> rusqlite::Result<usize> {
        self.conn
            .execute("DELETE FROM events WHERE at < ?1", params![before])
    }

    /// Every saved state row, as (key, JSON). Empty on a fresh database, or one that still has the older single-text save.
    pub fn load_state(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT key, body FROM state")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Saves the core's state, replacing the previous save, in one transaction.
    pub fn save_snapshot(&mut self, body: &str, at: i64) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO snapshot (id, at, body) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET at = ?1, body = ?2", params![at, body])?;
        Ok(())
    }

    /// The last saved state, if there is one.
    pub fn load_snapshot(&self) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT body FROM snapshot WHERE id = 1", [], |r| r.get(0))
            .optional()
    }

    /// Adds many history rows in ONE transaction (group commit): either all of them are saved or none are. Ids are
    /// assigned by the database, so they rise and never repeat, even across restarts.
    pub fn append(&mut self, rows: &[HistoryRow]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT INTO history (at, project, thread, sender, kind, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for r in rows {
                st.execute(params![r.at, r.project, r.thread, r.from, r.kind, r.text])?;
            }
        }
        tx.commit()
    }

    /// Records a decision for the audit trail.
    pub fn audit(&mut self, at: i64, project: &str, who: &str, what: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO audit (at, project, who, what) VALUES (?1, ?2, ?3, ?4)",
            params![at, project, who, what],
        )?;
        Ok(())
    }

    /// History rows of a project (optionally one thread) with id above `after`, oldest first, at most `limit`. Reads
    /// the database only. Older days are in compressed files, see `read_segment`.
    pub fn history(
        &self,
        project: &str,
        thread: Option<&str>,
        after: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<HistoryRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, at, project, thread, sender, kind, body FROM history
             WHERE project = ?1 AND id > ?2 AND (?3 IS NULL OR thread = ?3) ORDER BY id LIMIT ?4",
        )?;
        let rows = st.query_map(params![project, after, thread, limit as i64], |r| {
            Ok(HistoryRow {
                id: r.get(0)?,
                at: r.get(1)?,
                project: r.get(2)?,
                thread: r.get(3)?,
                from: r.get(4)?,
                kind: r.get(5)?,
                text: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// The newest `limit` history rows of a project (optionally one thread), oldest of them first. Reads the database only.
    pub fn history_latest(
        &self,
        project: &str,
        thread: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<HistoryRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, at, project, thread, sender, kind, body FROM history
             WHERE project = ?1 AND (?2 IS NULL OR thread = ?2) ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![project, thread, limit as i64], |r| {
            Ok(HistoryRow {
                id: r.get(0)?,
                at: r.get(1)?,
                project: r.get(2)?,
                thread: r.get(3)?,
                from: r.get(4)?,
                kind: r.get(5)?,
                text: r.get(6)?,
            })
        })?;
        let mut out: Vec<HistoryRow> = rows.collect::<Result<_, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// Number of history rows still in the database.
    pub fn hot_rows(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
    }

    /// Moves history older than `before` out of the database into compressed files, one file per project per UTC day (and per
    /// `ROLLOVER_CHUNK` rows if a day is huge, so memory stays small). A file is never changed once written. With a bucket set up, a
    /// file is uploaded and verified before anything is deleted locally, so a failed upload leaves the history in the database and
    /// reports the error: nothing is ever lost to a bad connection. Returns how many rows moved.
    pub fn rollover(&mut self, before: i64) -> std::io::Result<usize> {
        self.rollover_chunked(before, ROLLOVER_CHUNK)
    }

    /// `rollover` with the largest file given (so tests can make small ones).
    pub fn rollover_chunked(&mut self, before: i64, chunk: usize) -> std::io::Result<usize> {
        let Some(dir) = self.segments_dir.clone() else {
            return Ok(0);
        };
        std::fs::create_dir_all(&dir)?;
        let buckets: Vec<(String, i64)> = {
            let mut st = self
                .conn
                .prepare(
                    "SELECT DISTINCT project, at / ?2 FROM history WHERE at < ?1 ORDER BY 1, 2",
                )
                .map_err(io)?;
            st.query_map(params![before, DAY_MS], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(io)?
                .collect::<Result<_, _>>()
                .map_err(io)?
        };
        let mut moved = 0;
        for (project, day) in buckets {
            let (lo, hi) = (day * DAY_MS, (day + 1) * DAY_MS);
            loop {
                let rows: Vec<HistoryRow> = {
                    let mut st = self
                        .conn
                        .prepare("SELECT id, at, project, thread, sender, kind, body FROM history WHERE project = ?1 AND at >= ?2 AND at < ?3 AND at < ?4 ORDER BY id LIMIT ?5")
                        .map_err(io)?;
                    st.query_map(params![project, lo, hi, before, chunk as i64], |r| {
                        Ok(HistoryRow {
                            id: r.get(0)?,
                            at: r.get(1)?,
                            project: r.get(2)?,
                            thread: r.get(3)?,
                            from: r.get(4)?,
                            kind: r.get(5)?,
                            text: r.get(6)?,
                        })
                    })
                    .map_err(io)?
                    .collect::<Result<_, _>>()
                    .map_err(io)?
                };
                if rows.is_empty() {
                    break;
                }
                let (first, last) = (rows[0].id, rows[rows.len() - 1].id);
                let file = segment_name(&project, day, first, last);
                let bytes = encode_segment(&rows)?;
                self.store_file(&dir, &file, &bytes)?;
                let tx = self.conn.transaction().map_err(io)?;
                tx.execute("INSERT OR REPLACE INTO segments (file, project, first_id, last_id, rows, day) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![file, project, first, last, rows.len() as i64, day]).map_err(io)?;
                tx.execute(
                    "DELETE FROM history WHERE project = ?1 AND id BETWEEN ?2 AND ?3 AND at >= ?4 AND at < ?5 AND at < ?6",
                    params![project, first, last, lo, hi, before],
                )
                .map_err(io)?;
                tx.commit().map_err(io)?;
                if self.bucket.is_some() {
                    let _ = std::fs::remove_file(dir.join(&file));
                }
                moved += rows.len();
                if rows.len() < chunk {
                    break;
                }
            }
        }
        Ok(moved)
    }

    /// Writes an encoded history file to the folder, and to the bucket if there is one (which must succeed before the caller deletes
    /// anything it replaces).
    fn store_file(&self, dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
        let mut f = std::fs::File::create(dir.join(file))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(b) = &self.bucket {
            b.put(file, bytes)?;
        }
        Ok(())
    }

    /// Joins the several small files a day can end up in (one per rollover that found a few new rows) into one, so the number of files
    /// grows with the number of days and not with how often the hub rolls over. Only days with at most `max_rows` rows in total are
    /// joined, so a merge never needs much memory. The new file is written, and uploaded if there is a bucket, before the old ones are
    /// removed. Returns how many files were removed.
    pub fn compact(&mut self, max_rows: usize) -> std::io::Result<usize> {
        let Some(dir) = self.segments_dir.clone() else {
            return Ok(0);
        };
        let days: Vec<(String, i64)> = {
            let mut st = self
                .conn
                .prepare("SELECT project, day FROM segments WHERE day >= 0 GROUP BY project, day HAVING COUNT(*) >= 2 AND SUM(rows) <= ?1")
                .map_err(io)?;
            st.query_map(params![max_rows as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(io)?
                .collect::<Result<_, _>>()
                .map_err(io)?
        };
        let mut removed = 0;
        for (project, day) in days {
            let old: Vec<Segment> = self
                .segments(&project)
                .map_err(io)?
                .into_iter()
                .filter(|s| s.day == day)
                .collect();
            let mut rows = Vec::new();
            for seg in &old {
                rows.extend(self.read_segment(&seg.file)?);
            }
            rows.sort_by_key(|r| r.id);
            rows.dedup_by_key(|r| r.id);
            let (first, last) = (rows[0].id, rows[rows.len() - 1].id);
            let file = segment_name(&project, day, first, last);
            self.store_file(&dir, &file, &encode_segment(&rows)?)?;
            let tx = self.conn.transaction().map_err(io)?;
            for seg in &old {
                tx.execute("DELETE FROM segments WHERE file = ?1", params![seg.file])
                    .map_err(io)?;
            }
            tx.execute("INSERT OR REPLACE INTO segments (file, project, first_id, last_id, rows, day) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![file, project, first, last, rows.len() as i64, day]).map_err(io)?;
            tx.commit().map_err(io)?;
            // Only now, with the merged file safe and the index pointing at it, are the old files removed.
            for seg in old.iter().filter(|s| s.file != file) {
                let _ = std::fs::remove_file(dir.join(&seg.file));
                if let Some(b) = &self.bucket {
                    let _ = b.delete(&seg.file);
                }
                removed += 1;
            }
            if self.bucket.is_some() {
                let _ = std::fs::remove_file(dir.join(&file));
            }
        }
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        Ok(removed)
    }

    /// The newest `limit` rows of a project (optionally one thread) older than `before` (or the newest of all), oldest of them first, read
    /// from the database and, when the database does not have enough, from the compressed files, newest file first. Paging back is
    /// `before = the id of the oldest row seen`. Files are unpacked once and kept for a few reads, so paging through a day is cheap.
    ///
    /// ponytail: rows are taken newest file first and the database's rows are assumed newer than the files' (true while rows arrive in
    /// order). A row stamped out of order could appear a page late; a merge by id across all sources fixes that if it ever matters.
    pub fn history_before(
        &self,
        project: &str,
        thread: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> std::io::Result<Vec<HistoryRow>> {
        let before = before.unwrap_or(i64::MAX);
        let mut out: Vec<HistoryRow> = {
            let mut st = self
                .conn
                .prepare_cached(
                    "SELECT id, at, project, thread, sender, kind, body FROM history
                     WHERE project = ?1 AND id < ?2 AND (?3 IS NULL OR thread = ?3) ORDER BY id DESC LIMIT ?4",
                )
                .map_err(io)?;
            st.query_map(params![project, before, thread, limit as i64], |r| {
                Ok(HistoryRow {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    project: r.get(2)?,
                    thread: r.get(3)?,
                    from: r.get(4)?,
                    kind: r.get(5)?,
                    text: r.get(6)?,
                })
            })
            .map_err(io)?
            .collect::<Result<_, _>>()
            .map_err(io)?
        };
        if out.len() < limit {
            let mut segs = self.segments(project).map_err(io)?;
            segs.retain(|s| s.first_id < before);
            segs.sort_by_key(|s| std::cmp::Reverse(s.first_id));
            'files: for seg in segs {
                for r in self.read_segment_cached(&seg.file)?.iter().rev() {
                    if r.id < before && thread.is_none_or(|t| r.thread.as_deref() == Some(t)) {
                        out.push(r.clone());
                        if out.len() >= limit {
                            break 'files;
                        }
                    }
                }
            }
        }
        out.reverse();
        Ok(out)
    }

    /// `read_segment`, keeping the last few files unpacked.
    fn read_segment_cached(&self, file: &str) -> std::io::Result<std::sync::Arc<Vec<HistoryRow>>> {
        {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((_, rows)) = cache.iter().find(|(f, _)| f == file) {
                return Ok(rows.clone());
            }
        }
        let rows = std::sync::Arc::new(self.read_segment(file)?);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.push_back((file.to_string(), rows.clone()));
        while cache.len() > SEGMENT_CACHE {
            cache.pop_front();
        }
        Ok(rows)
    }

    /// The compressed files that hold part of a project's history.
    pub fn segments(&self, project: &str) -> rusqlite::Result<Vec<Segment>> {
        let mut st = self.conn.prepare("SELECT file, project, first_id, last_id, rows, day FROM segments WHERE project = ?1 ORDER BY first_id")?;
        let rows = st.query_map(params![project], |r| {
            Ok(Segment {
                file: r.get(0)?,
                project: r.get(1)?,
                first_id: r.get(2)?,
                last_id: r.get(3)?,
                rows: r.get(4)?,
                day: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Reads one compressed history file back: from local disk if it is still there, otherwise from the bucket.
    pub fn read_segment(&self, file: &str) -> std::io::Result<Vec<HistoryRow>> {
        let name = crate::agents::text::safe_name(file);
        let local = self.segments_dir.as_ref().map(|d| d.join(&name));
        let bytes = match local.as_ref().map(std::fs::read) {
            Some(Ok(b)) => b,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound && self.bucket.is_some() => {
                self.bucket.as_ref().expect("checked").get(&name)?
            }
            Some(Err(e)) => return Err(e),
            None => return Err(std::io::Error::other("no segments folder")),
        };
        let mut text = String::new();
        GzDecoder::new(&bytes[..]).read_to_string(&mut text)?;
        text.lines()
            .map(|l| serde_json::from_str(l).map_err(std::io::Error::other))
            .collect()
    }

    /// Copies every history file from one bucket to another (for example from Oracle to Cloudflare R2), verifying each
    /// copy by reading it back. Does not delete anything from the old bucket. Returns how many files were copied.
    pub fn migrate_segments(
        &self,
        from: &bucket::Bucket,
        to: &bucket::Bucket,
    ) -> std::io::Result<usize> {
        let mut st = self.conn.prepare("SELECT file FROM segments").map_err(io)?;
        let files: Vec<String> = st
            .query_map([], |r| r.get(0))
            .map_err(io)?
            .collect::<Result<_, _>>()
            .map_err(io)?;
        for f in &files {
            let bytes = from.get(f)?;
            to.put(f, &bytes)?;
            if to.get(f)? != bytes {
                return Err(std::io::Error::other(format!("{f} did not copy correctly")));
            }
        }
        Ok(files.len())
    }
}

impl Store {
    /// Makes a new secret token for a device and saves only its hash. The token is returned once and cannot be
    /// recovered later, so a copy of the database does not let anyone act as a device.
    pub fn create_token(&mut self, node: &str, at: i64) -> rusqlite::Result<String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the system has a random source");
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.conn.execute(
            "INSERT INTO tokens (hash, node, at) VALUES (?1, ?2, ?3)",
            params![hash_token(&token), node, at],
        )?;
        Ok(token)
    }

    /// Which device a token belongs to, if it is a real, unrevoked token.
    pub fn node_for_token(&self, token: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT node FROM tokens WHERE hash = ?1",
                params![hash_token(token)],
                |r| r.get(0),
            )
            .optional()
    }

    /// Cancels every token a device has. It can no longer connect.
    pub fn revoke_node(&mut self, node: &str) -> rusqlite::Result<usize> {
        self.conn
            .execute("DELETE FROM tokens WHERE node = ?1", params![node])
    }
}

/// The form a token is stored in: its SHA-256, in hex.
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Wraps a database error as an I/O error, for functions that mix both.
fn io(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}
