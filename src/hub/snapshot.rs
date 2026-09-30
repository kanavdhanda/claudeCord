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
            owners: self.owners.clone(),
            members: self.members.clone(),
            agents: self.agents.clone(),
            by_project: self.by_project.clone(),
            asks: self.asks.clone(),
            perms: self.perms.clone(),
            grants: self.grants.clone(),
            tasks: self.tasks.clone(),
            queues: self.queues.clone(),
            pending: self
                .pending
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            briefed: self.briefed.clone(),
            roster_dirty: self.roster_dirty.clone(),
            handoffs: self.handoffs.clone(),
            dump_asked: self.dump_asked.clone(),
            counters: self.counters.clone(),
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
        self.owners = s.owners;
        self.members = s.members;
        self.status = s
            .agents
            .keys()
            .map(|id| (id.clone(), (AgentStatus::Offline, None)))
            .collect();
        self.agents = s.agents;
        self.by_project = s.by_project;
        self.asks = s.asks;
        self.perms = s.perms;
        self.grants = s.grants;
        self.tasks = s.tasks;
        self.queues = s.queues;
        self.pending = s.pending;
        self.briefed = s.briefed;
        self.roster_dirty = s.roster_dirty;
        self.handoffs = s.handoffs;
        self.dump_asked = s.dump_asked;
        self.counters = s.counters;
        self.seq = s.seq;
        true
    }
}
