//! The uptime log: for each component, when its state changed. Only real changes are written, so the table stays tiny
//! (a component that flaps every minute for a year is about half a million rows). See `crate::uptime` for how it is read.

use super::Store;
use crate::uptime::{Change, State};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// The newest recorded state of a component.
    pub fn uptime_last(&self, component: &str) -> rusqlite::Result<Option<State>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM uptime WHERE component = ?1 ORDER BY at DESC, rowid DESC LIMIT 1",
                params![component],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| State::parse(&s)))
    }

    /// Records a state, unless it is already the newest one. Returns whether anything was written.
    pub fn uptime_set(&self, component: &str, state: State, at: i64) -> rusqlite::Result<bool> {
        if self.uptime_last(component)? == Some(state) {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO uptime (component, at, state) VALUES (?1, ?2, ?3)",
            params![component, at, state.as_str()],
        )?;
        Ok(true)
    }

    /// The changes of a component from the last one at or before `since` onward, oldest first.
    pub fn uptime_changes(&self, component: &str, since: i64) -> rusqlite::Result<Vec<Change>> {
        let mut q = self.conn.prepare(
            "SELECT at, state FROM uptime WHERE component = ?1 AND at >= COALESCE(
                 (SELECT MAX(at) FROM uptime WHERE component = ?1 AND at <= ?2), ?2)
             ORDER BY at, rowid",
        )?;
        let rows = q.query_map(params![component, since], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(at, s)| State::parse(&s).map(|state| Change { at, state }))
            .collect())
    }

    /// Every component that has a record.
    pub fn uptime_components(&self) -> rusqlite::Result<Vec<String>> {
        let mut q = self
            .conn
            .prepare("SELECT DISTINCT component FROM uptime ORDER BY component")?;
        let rows = q.query_map([], |r| r.get(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }
}
