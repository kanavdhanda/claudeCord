//! Saving and restoring the core's durable state. The core keeps everything in memory for speed, and this turns the
//! parts that must survive a restart into one piece of text and back: who the people are, which agents exist, open
//! asks and permissions, standing grants, tasks, handoffs, and every message still waiting to be delivered.
//!
//! What is deliberately not saved: device connections (devices reconnect on their own), file uploads in progress (the
//! agent resends), and metrics (saved separately). After a restore every agent counts as offline until its device
//! registers again, at which point its waiting messages are delivered.

use super::core::{HubCore, Pending, Queued};
use super::model::*;
use crate::protocol::AgentStatus;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Version of the saved format, so a future change can read old saves.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    owners: HashSet<String>,
    members: HashMap<String, HashMap<String, Member>>,
    agents: HashMap<String, AgentRow>,
    by_project: HashMap<String, Vec<String>>,
    asks: HashMap<String, Vec<Ask>>,
    perms: HashMap<String, Vec<PermRequest>>,
    grants: Vec<Grant>,
    tasks: HashMap<String, Vec<TaskRow>>,
    queues: HashMap<String, Vec<Queued>>,
    pending: HashMap<String, Pending>,
    briefed: HashSet<String>,
    roster_dirty: HashSet<String>,
    handoffs: HashMap<String, Handoff>,
    dump_asked: HashMap<String, i64>,
    counters: HashMap<String, u32>,
    seq: u64,
}

impl HubCore {
    /// The durable state as JSON text. Cheap enough to call after every batch of changes at small scale, and the store
    /// calls it on a short timer when anything changed.
    pub fn snapshot(&self) -> String {
        let s = Snapshot {
            version: VERSION,
            owners: (*self.owners).clone(),
            members: (*self.members).clone(),
            agents: (*self.agents).clone(),
            by_project: (*self.by_project).clone(),
            asks: (*self.asks).clone(),
            perms: (*self.perms).clone(),
            grants: (*self.grants).clone(),
            tasks: (*self.tasks).clone(),
            queues: (*self.queues).clone(),
            pending: self
                .pending
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            briefed: (*self.briefed).clone(),
            roster_dirty: (*self.roster_dirty).clone(),
            handoffs: (*self.handoffs).clone(),
            dump_asked: (*self.dump_asked).clone(),
            counters: (*self.counters).clone(),
            seq: self.seq,
        };
        serde_json::to_string(&s).expect("snapshot is plain data")
    }

    /// Rebuilds the durable state from text made by `snapshot`. Returns false and changes nothing if the text is not a
    /// readable snapshot of a version this code understands.
    pub fn restore(&mut self, text: &str) -> bool {
        let Ok(s) = serde_json::from_str::<Snapshot>(text) else {
            return false;
        };
        if s.version != VERSION {
            return false;
        }
        self.status = s
            .agents
            .keys()
            .map(|id| (id.clone(), (AgentStatus::Offline, None)))
            .collect();
        *self.owners = s.owners;
        self.members.load(s.members);
        self.agents.load(s.agents);
        self.by_project.load(s.by_project);
        self.asks.load(s.asks);
        self.perms.load(s.perms);
        *self.grants = s.grants;
        self.tasks.load(s.tasks);
        self.queues.load(s.queues);
        self.pending.load(s.pending);
        self.briefed.load(s.briefed);
        self.roster_dirty.load(s.roster_dirty);
        self.handoffs.load(s.handoffs);
        self.dump_asked.load(s.dump_asked);
        self.counters.load(s.counters);
        self.seq = s.seq;
        // What was just loaded is what is on disk; only changes from here on need saving.
        self.forget_changes();
        true
    }
}

pub use crate::store::Changes;

impl HubCore {
    /// The rows that changed since the last call, worked out from what the tracked collections noted (see `tracked`), so what a save
    /// costs is what changed and not how much state there is. Calling it starts a new round of noting.
    pub fn take_changes(&mut self) -> Changes {
        let taken = std::mem::take(
            &mut *self
                .dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut out = Changes::default();
        let mut by_name: HashMap<&'static str, Vec<String>> = HashMap::new();
        for (name, key) in taken.keys {
            if !taken.whole.contains(name) {
                by_name.entry(name).or_default().push(key);
            }
        }
        for (name, touched) in by_name {
            let (up, del) = self.rows_for(name, &touched);
            out.upserts.extend(up);
            out.deletes.extend(del);
        }
        for name in taken.whole {
            if name == "owners" || name == "grants" {
                out.upserts.extend(self.all_rows_for(name));
            } else {
                out.clears.push(format!("{name}:"));
                out.upserts.extend(self.all_rows_for(name));
            }
        }
        if self.seq != self.seq_written || !out.is_empty() {
            self.seq_written = self.seq;
            out.upserts.push(("seq".into(), self.seq.to_string()));
        }
        out
    }

    /// Drops what has been noted as changed (used after loading, when memory and disk agree).
    pub(super) fn forget_changes(&mut self) {
        *self
            .dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Default::default();
        self.seq_written = self.seq;
    }

    /// Notes every collection as wholly changed, so the next `take_changes` writes all of the state (used after loading an old-format
    /// save, to move it into rows).
    pub fn mark_all_dirty(&mut self) {
        let mut log = self
            .dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in COLLECTIONS {
            log.whole.insert(name);
        }
        self.seq_written = u64::MAX;
    }

    fn rows_for(&self, name: &str, touched: &[String]) -> (Vec<(String, String)>, Vec<String>) {
        match name {
            "agents" => self.agents.rows(touched),
            "by_project" => self.by_project.rows(touched),
            "members" => self.members.rows(touched),
            "asks" => self.asks.rows(touched),
            "perms" => self.perms.rows(touched),
            "tasks" => self.tasks.rows(touched),
            "queues" => self.queues.rows(touched),
            "pending" => self.pending.rows(touched),
            "handoffs" => self.handoffs.rows(touched),
            "dump_asked" => self.dump_asked.rows(touched),
            "counters" => self.counters.rows(touched),
            "device_seq" => self.device_seq.rows(touched),
            "briefed" => self.briefed.rows(touched),
            "roster_dirty" => self.roster_dirty.rows(touched),
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn all_rows_for(&self, name: &str) -> Vec<(String, String)> {
        match name {
            "agents" => self.agents.all_rows(),
            "by_project" => self.by_project.all_rows(),
            "members" => self.members.all_rows(),
            "asks" => self.asks.all_rows(),
            "perms" => self.perms.all_rows(),
            "tasks" => self.tasks.all_rows(),
            "queues" => self.queues.all_rows(),
            "pending" => self.pending.all_rows(),
            "handoffs" => self.handoffs.all_rows(),
            "dump_asked" => self.dump_asked.all_rows(),
            "counters" => self.counters.all_rows(),
            "device_seq" => self.device_seq.all_rows(),
            "briefed" => self.briefed.all_rows(),
            "roster_dirty" => self.roster_dirty.all_rows(),
            "owners" => vec![self.owners.row()],
            "grants" => vec![self.grants.row()],
            _ => Vec::new(),
        }
    }

    /// Rebuilds the durable state from saved rows (`collection:key` and a JSON body). A row that cannot be read is skipped and logged,
    /// so one bad row never stops the hub from starting with everything else.
    pub fn restore_rows(&mut self, rows: Vec<(String, String)>) {
        fn parse<T: serde::de::DeserializeOwned>(key: &str, body: &str) -> Option<T> {
            match serde_json::from_str(body) {
                Ok(v) => Some(v),
                Err(e) => {
                    crate::warn!(
                        "hub",
                        "skipped a saved row that could not be read ({key}): {e}"
                    );
                    None
                }
            }
        }
        let mut agents = HashMap::new();
        let mut by_project = HashMap::new();
        let mut members = HashMap::new();
        let mut asks = HashMap::new();
        let mut perms = HashMap::new();
        let mut tasks = HashMap::new();
        let mut queues = HashMap::new();
        let mut pending = HashMap::new();
        let mut handoffs = HashMap::new();
        let mut dump_asked = HashMap::new();
        let mut counters = HashMap::new();
        let mut device_seq = HashMap::new();
        let mut briefed = HashSet::new();
        let mut roster_dirty = HashSet::new();
        for (row, body) in rows {
            match row.split_once(':') {
                None => match row.as_str() {
                    "owners" => {
                        if let Some(v) = parse(&row, &body) {
                            *self.owners = v;
                        }
                    }
                    "grants" => {
                        if let Some(v) = parse(&row, &body) {
                            *self.grants = v;
                        }
                    }
                    "seq" => self.seq = body.parse().unwrap_or(0),
                    _ => {}
                },
                Some((name, key)) => {
                    let key = key.to_string();
                    match name {
                        "agents" => agents.extend(parse(&row, &body).map(|v: AgentRow| (key, v))),
                        "by_project" => {
                            by_project.extend(parse(&row, &body).map(|v: Vec<String>| (key, v)))
                        }
                        "members" => members
                            .extend(parse(&row, &body).map(|v: HashMap<String, Member>| (key, v))),
                        "asks" => asks.extend(parse(&row, &body).map(|v: Vec<Ask>| (key, v))),
                        "perms" => {
                            perms.extend(parse(&row, &body).map(|v: Vec<PermRequest>| (key, v)))
                        }
                        "tasks" => tasks.extend(parse(&row, &body).map(|v: Vec<TaskRow>| (key, v))),
                        "queues" => {
                            queues.extend(parse(&row, &body).map(|v: Vec<Queued>| (key, v)))
                        }
                        "pending" => pending.extend(parse(&row, &body).map(|v: Pending| (key, v))),
                        "handoffs" => {
                            handoffs.extend(parse(&row, &body).map(|v: Handoff| (key, v)))
                        }
                        "dump_asked" => {
                            dump_asked.extend(parse(&row, &body).map(|v: i64| (key, v)))
                        }
                        "counters" => counters.extend(parse(&row, &body).map(|v: u32| (key, v))),
                        "device_seq" => {
                            device_seq.extend(parse(&row, &body).map(|v: [u64; 2]| (key, v)))
                        }
                        "briefed" => {
                            briefed.insert(key);
                        }
                        "roster_dirty" => {
                            roster_dirty.insert(key);
                        }
                        _ => {}
                    }
                }
            }
        }
        self.status = agents
            .keys()
            .map(|id: &String| (id.clone(), (AgentStatus::Offline, None)))
            .collect();
        self.agents.load(agents);
        self.by_project.load(by_project);
        self.members.load(members);
        self.asks.load(asks);
        self.perms.load(perms);
        self.tasks.load(tasks);
        self.queues.load(queues);
        self.pending.load(pending);
        self.handoffs.load(handoffs);
        self.dump_asked.load(dump_asked);
        self.counters.load(counters);
        self.device_seq.load(device_seq);
        self.briefed.load(briefed);
        self.roster_dirty.load(roster_dirty);
        self.forget_changes();
    }
}

/// Every collection that is saved as rows.
const COLLECTIONS: [&str; 16] = [
    "agents",
    "by_project",
    "members",
    "asks",
    "perms",
    "tasks",
    "queues",
    "pending",
    "handoffs",
    "dump_asked",
    "counters",
    "device_seq",
    "briefed",
    "roster_dirty",
    "owners",
    "grants",
];
