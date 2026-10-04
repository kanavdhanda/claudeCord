//! Counters for the dashboard's insights. Recording an event is one array write, so it costs nothing on the message
//! path. Values live in a ring of one-minute buckets covering a day, and are saved sparsely, so a restart keeps
//! history and the saved form stays tiny. Every method takes the time as an argument, so tests need no clock.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

const MINUTE: i64 = 60_000;

/// Upper bounds, in ms, of the acceptance latency buckets. Anything slower lands in the last one.
pub const ACCEPT_BOUNDS: [f64; 5] = [1000.0, 5000.0, 15_000.0, 60_000.0, f64::INFINITY];
const ACCEPT_KEYS: [&str; 5] = [
    "accept_1",
    "accept_5",
    "accept_15",
    "accept_60",
    "accept_slow",
];

struct Ring {
    vals: Vec<f64>,
    stamp: Vec<i64>,
}

impl Ring {
    /// An empty ring that can hold `size` minutes of counts.
    fn new(size: usize) -> Self {
        Self {
            vals: vec![0.0; size],
            stamp: vec![-1; size],
        }
    }

    /// Which position in the ring a given minute lands in. Old minutes wrap around and overwrite.
    fn slot(&self, minute: i64) -> usize {
        minute.rem_euclid(self.vals.len() as i64) as usize
    }

    /// Adds to a minute, first clearing the slot if it still holds an older minute.
    fn add(&mut self, minute: i64, n: f64) {
        let i = self.slot(minute);
        if self.stamp[i] != minute {
            self.stamp[i] = minute;
            self.vals[i] = 0.0;
        }
        self.vals[i] += n;
    }

    /// The count for a minute, or zero if that minute is no longer (or not yet) in the ring.
    fn get(&self, minute: i64) -> f64 {
        let i = self.slot(minute);
        if self.stamp[i] == minute {
            self.vals[i]
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Acceptance {
    pub n: f64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub buckets: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCycle {
    pub n: f64,
    pub avg_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Insights {
    pub range_minutes: usize,
    pub bucket_minutes: usize,
    pub start: i64,
    pub series: BTreeMap<String, Vec<f64>>,
    pub totals: BTreeMap<String, f64>,
    pub acceptance: Acceptance,
    pub task_cycle: TaskCycle,
}

struct Busy {
    ms: i64,
    since: Option<i64>,
    /// Order in which the agent was first seen, so ties rank the same way every time.
    order: u64,
}

pub struct Metrics {
    minutes: usize,
    rings: HashMap<String, Ring>,
    busy: HashMap<String, Busy>,
    seen: u64,
}

impl Default for Metrics {
    /// A day of one-minute buckets.
    fn default() -> Self {
        Self::new(1440)
    }
}

impl Metrics {
    /// Metrics that remember the last `minutes` minutes.
    pub fn new(minutes: usize) -> Self {
        Self {
            minutes,
            rings: HashMap::new(),
            busy: HashMap::new(),
            seen: 0,
        }
    }

    /// Adds `n` to the named counter for the current minute.
    pub fn inc(&mut self, key: &str, n: f64, now: i64) {
        let size = self.minutes;
        self.rings
            .entry(key.to_string())
            .or_insert_with(|| Ring::new(size))
            .add(now.div_euclid(MINUTE), n);
    }

    /// How long an agent took to pick up a message.
    pub fn accepted(&mut self, ms: f64, now: i64) {
        let i = ACCEPT_BOUNDS
            .iter()
            .position(|&b| ms < b)
            .unwrap_or(ACCEPT_KEYS.len() - 1);
        self.inc(ACCEPT_KEYS[i], 1.0, now);
        self.inc("accept_sum_ms", ms, now);
    }

    /// Records a finished task and how long it took from assignment to done.
    pub fn task_finished(&mut self, cycle_ms: f64, now: i64) {
        self.inc("task_done", 1.0, now);
        self.inc("task_cycle_ms", cycle_ms, now);
    }

    /// Tracks how long each agent spends working, from its status changes.
    pub fn status(&mut self, agent_id: &str, status: &str, now: i64) {
        let order = self.seen;
        let b = self
            .busy
            .entry(agent_id.to_string())
            .or_insert_with(|| Busy {
                ms: 0,
                since: None,
                order,
            });
        if b.order == order {
            self.seen += 1;
        }
        let working = status == "thinking" || status == "executing";
        match (working, b.since) {
            (true, None) => b.since = Some(now),
            (false, Some(since)) => {
                b.ms += now - since;
                b.since = None;
            }
            _ => {}
        }
    }

    /// Stops tracking an agent, for example because it left.
    pub fn forget(&mut self, agent_id: &str) {
        self.busy.remove(agent_id);
    }

    /// The agents that spent the most time working, most first, including a stretch still in progress.
    pub fn busiest(&self, limit: usize, now: i64) -> Vec<(String, i64)> {
        let mut v: Vec<_> = self
            .busy
            .iter()
            .map(|(id, b)| (b.order, id.clone(), b.ms + b.since.map_or(0, |s| now - s)))
            .filter(|x| x.2 > 0)
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        v.into_iter()
            .take(limit)
            .map(|(_, id, ms)| (id, ms))
            .collect()
    }

    /// Total of a counter over a range of minutes.
    fn sum(&self, key: &str, from: i64, to: i64) -> f64 {
        self.rings
            .get(key)
            .map_or(0.0, |r| (from..=to).map(|m| r.get(m)).sum())
    }

    /// A chart-ready view of the last `range_minutes`, in buckets of `bucket_minutes`.
    pub fn insights(
        &self,
        range_minutes: usize,
        bucket_minutes: usize,
        series_keys: &[&str],
        now: i64,
    ) -> Insights {
        let range = range_minutes.min(self.minutes);
        let end = now.div_euclid(MINUTE);
        let start = end - range as i64 + 1;
        let n = range.div_ceil(bucket_minutes);
        let mut series = BTreeMap::new();
        for k in series_keys {
            let mut out = vec![0.0; n];
            if let Some(r) = self.rings.get(*k) {
                for m in start..=end {
                    let idx = (((m - start) as usize) / bucket_minutes).min(n - 1);
                    out[idx] += r.get(m);
                }
            }
            series.insert((*k).to_string(), out);
        }
        let totals: BTreeMap<String, f64> = self
            .rings
            .keys()
            .map(|k| (k.clone(), self.sum(k, start, end)))
            .collect();
        let buckets: Vec<f64> = ACCEPT_KEYS
            .iter()
            .map(|k| totals.get(*k).copied().unwrap_or(0.0))
            .collect();
        let count: f64 = buckets.iter().sum();
        let pct = |q: f64| -> Option<f64> {
            if count == 0.0 {
                return None;
            }
            let mut cum = 0.0;
            for (i, b) in buckets.iter().enumerate() {
                cum += b;
                if cum / count >= q {
                    return Some(if ACCEPT_BOUNDS[i].is_infinite() {
                        ACCEPT_BOUNDS[3]
                    } else {
                        ACCEPT_BOUNDS[i]
                    });
                }
            }
            None
        };
        let done = totals.get("task_done").copied().unwrap_or(0.0);
        let cycle = totals.get("task_cycle_ms").copied().unwrap_or(0.0);
        Insights {
            range_minutes: range,
            bucket_minutes,
            start: start * MINUTE,
            series,
            acceptance: Acceptance {
                n: count,
                p50_ms: pct(0.5),
                p95_ms: pct(0.95),
                buckets,
            },
            task_cycle: TaskCycle {
                n: done,
                avg_ms: (done > 0.0).then(|| (cycle / done).round()),
            },
            totals,
        }
    }

    /// Only the non-zero buckets, so the saved form is small however long the hub has run.
    pub fn to_json(&self, now: i64) -> BTreeMap<String, BTreeMap<i64, f64>> {
        let end = now.div_euclid(MINUTE);
        let mut out = BTreeMap::new();
        for (k, r) in &self.rings {
            let m: BTreeMap<i64, f64> = ((end - self.minutes as i64 + 1)..=end)
                .filter_map(|t| Some(t).zip(Some(r.get(t))).filter(|(_, v)| *v != 0.0))
                .collect();
            if !m.is_empty() {
                out.insert(k.clone(), m);
            }
        }
        out
    }

    /// Restores saved data, ignoring anything old, from the future, or malformed.
    pub fn load(&mut self, data: &serde_json::Value, now: i64) {
        let end = now.div_euclid(MINUTE);
        let Some(map) = data.as_object() else { return };
        for (k, m) in map {
            let Some(m) = m.as_object() else { continue };
            for (t, v) in m {
                let (Ok(minute), Some(v)) = (t.parse::<i64>(), v.as_f64()) else {
                    continue;
                };
                if v.is_finite() && minute > end - self.minutes as i64 && minute <= end {
                    self.inc(k, v, minute * MINUTE);
                }
            }
        }
    }
}
