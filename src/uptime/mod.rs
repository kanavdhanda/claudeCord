//! Uptime: how much of the time each part of claudeCord was working, measured the way production services do it.
//!
//! What is kept is a log of state changes per component (`hub`, `discord`, and `external:<name>` for a prober on another
//! machine), not a sample per second. Availability over a window is worked out from that log: the time spent up divided by the
//! time the component was known to be up or down. Time before the first record counts as unknown and is left out, never
//! counted as up. The error budget is what a target allows: 99.9% over 30 days allows about 43 minutes down.
//!
//! The hub cannot record its own sudden death, so while it runs it writes a heartbeat. At the next start, a component still
//! marked up whose heartbeat stopped is marked down from that heartbeat, which makes crashes count as downtime.
//!
//! `report` and `budget_left` are pure (the time is an argument); the rest talks to the store.

use crate::store::Store;
use std::time::Duration;

/// A component's state. Degraded is left out on purpose: nothing here can tell "slow" from "working" without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Up,
    Down,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

/// A recorded change of state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub at: i64,
    pub state: State,
}

/// What a window of time looked like.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Report {
    pub up_ms: i64,
    pub down_ms: i64,
    /// Time in the window before anything was recorded.
    pub unknown_ms: i64,
}

impl Report {
    /// Share of known time spent up, 0 to 1. None when nothing is known yet.
    pub fn availability(&self) -> Option<f64> {
        let known = self.up_ms + self.down_ms;
        (known > 0).then(|| self.up_ms as f64 / known as f64)
    }
}

/// Works out a window from the recorded changes, which must be oldest first and may start before the window (the last one at
/// or before `since` says what state the window began in).
pub fn report(changes: &[Change], since: i64, now: i64) -> Report {
    let mut r = Report::default();
    let mut cursor = since;
    let mut state = changes
        .iter()
        .rev()
        .find(|c| c.at <= since)
        .map(|c| c.state);
    for c in changes.iter().filter(|c| c.at > since && c.at < now) {
        add(&mut r, state, c.at - cursor);
        cursor = c.at;
        state = Some(c.state);
    }
    add(&mut r, state, now - cursor);
    r
}

fn add(r: &mut Report, state: Option<State>, ms: i64) {
    match state {
        Some(State::Up) => r.up_ms += ms,
        Some(State::Down) => r.down_ms += ms,
        None => r.unknown_ms += ms,
    }
}

/// How much downtime the target still allows over the window, in ms. Negative means the budget is spent.
pub fn budget_left(r: &Report, target: f64) -> i64 {
    let window = r.up_ms + r.down_ms + r.unknown_ms;
    ((1.0 - target) * window as f64) as i64 - r.down_ms
}

/// The stretches of downtime in a window, oldest first, as (start, end).
pub fn outages(changes: &[Change], since: i64, now: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut down_from = changes
        .iter()
        .rev()
        .find(|c| c.at <= since)
        .filter(|c| c.state == State::Down)
        .map(|_| since);
    for c in changes.iter().filter(|c| c.at > since && c.at < now) {
        match (c.state, down_from) {
            (State::Down, None) => down_from = Some(c.at),
            (State::Up, Some(from)) => {
                out.push((from, c.at));
                down_from = None;
            }
            _ => {}
        }
    }
    out.extend(down_from.map(|from| (from, now)));
    out
}

/// The heartbeat key that shows when the hub last ran.
const ALIVE: &str = "hub_alive_at";
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Called when the hub starts. If the last run did not end cleanly (components still up, heartbeat stopped), they are marked
/// down from the last heartbeat. Then the hub is marked up.
pub fn recover(store: &Store, now: i64) {
    let beat = store
        .kv_get(ALIVE)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    if let Some(beat) = beat {
        let mut crashed = false;
        for c in store.uptime_components().unwrap_or_default() {
            if c.starts_with("external:") {
                continue;
            }
            if store.uptime_last(&c).ok().flatten() == Some(State::Up) {
                let _ = store.uptime_set(&c, State::Down, beat);
                crashed = true;
            }
        }
        if crashed {
            crate::warn!(
                "uptime",
                "the last run did not stop cleanly; counting it as down from its last heartbeat at {}",
                iso(beat)
            );
        }
    }
    let _ = store.uptime_set("hub", State::Up, now);
    let _ = store.kv_set(ALIVE, &now.to_string());
}

/// Records that the hub stopped on purpose. Everything the hub owns goes down with it.
pub fn stopped(store: &Store, now: i64) {
    for c in store.uptime_components().unwrap_or_default() {
        if !c.starts_with("external:") {
            let _ = store.uptime_set(&c, State::Down, now);
        }
    }
}

/// Writes the heartbeat every few seconds from a thread of its own (a database write can wait on the disk, which must never hold up the
/// async runtime) until it is dropped.
pub struct Heartbeat {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn heartbeat(store: Store) -> Heartbeat {
    let (stop, rx) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("hub-heartbeat".into())
        .spawn(move || {
            // Waiting on the channel is the sleep: dropping the sender wakes it at once, so stopping never waits out an interval.
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(HEARTBEAT) {
                if let Err(e) = store.kv_set(ALIVE, &crate::now_ms().to_string()) {
                    crate::warn!("uptime", "could not write the heartbeat: {e}");
                }
            }
        })
        .ok();
    Heartbeat {
        stop: Some(stop),
        thread,
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One check from the outside: does `base`/readyz answer 200 within the time? This is what a prober on another machine runs.
pub async fn probe_once(base: &str, timeout: Duration) -> bool {
    let Ok(http) = reqwest::Client::builder().timeout(timeout).build() else {
        return false;
    };
    http.get(format!("{}/readyz", base.trim_end_matches('/')))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Debounces a stream of checks the way production probers do: a failure only counts as an outage after `needed` in a row,
/// and the outage is dated from the first failure, not from when it was confirmed.
pub struct Debounce {
    needed: u32,
    first_failure: Option<i64>,
    fails: u32,
    state: Option<State>,
}

impl Debounce {
    pub fn new(needed: u32) -> Self {
        Self {
            needed,
            first_failure: None,
            fails: 0,
            state: None,
        }
    }

    /// Feeds one check. Returns a change of state to record, with the time it should be dated at.
    pub fn check(&mut self, ok: bool, now: i64) -> Option<Change> {
        if ok {
            self.fails = 0;
            self.first_failure = None;
            if self.state != Some(State::Up) {
                self.state = Some(State::Up);
                return Some(Change {
                    at: now,
                    state: State::Up,
                });
            }
        } else {
            self.fails += 1;
            self.first_failure.get_or_insert(now);
            if self.fails >= self.needed && self.state != Some(State::Down) {
                self.state = Some(State::Down);
                return Some(Change {
                    at: self.first_failure.unwrap_or(now),
                    state: State::Down,
                });
            }
        }
        None
    }
}

/// A time in milliseconds since 1970 as `2026-10-04 14:05:09 UTC`, worked out by hand (no calendar library needed).
pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970 to a calendar date (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
