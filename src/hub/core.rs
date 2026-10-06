//! The hub core itself: the struct that owns all live state, how devices connect and disconnect, how agents register,
//! and the single entry point for frames arriving from devices. Behaviour that belongs to one topic (asks, tasks,
//! permissions, routing, files, controls) lives in its own file as more `impl HubCore` blocks.
//!
//! The core is a functional core: it reads no clock and does no I/O. Every method takes the time as an argument and
//! returns the effects the caller should carry out.

use super::briefs;
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::agents::text::is_sensitive_path;
use crate::metrics::Metrics;
use crate::protocol::{AgentSpec, AgentStatus, HubFrame, NodeFrame};
use crate::security::redact::redact;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Agent-only messages in a row before an agent's forwarding pauses until a human speaks.
pub const DEFAULT_STREAK_LIMIT: u32 = 20;
/// How long a human message may sit unaccepted before the room is told.
pub const DEFAULT_ACCEPT_TIMEOUT_MS: u64 = 30_000;
/// How long a standing permission lasts unless the person says otherwise.
pub const DEFAULT_GRANT_TTL_MS: i64 = 60 * 60_000;

/// One message waiting for an agent. Several of these become one input when delivered.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Queued {
    pub from: String,
    pub text: String,
    pub thread: Option<String>,
    /// Chat reference of the human message, to confirm once accepted.
    pub reference: Option<String>,
    pub task_id: Option<String>,
    /// Set when this item is a handoff, so accepting it marks the handoff consumed.
    pub handoff: Option<u32>,
    /// Whether this item is worth an agent turn by itself. Items that only inform ride along with the next one
    /// that does, because every turn rereads the whole context and costs far more than the words.
    pub wake: bool,
    /// When it was queued, so an informing item that waits too long is still delivered.
    pub at: i64,
}

/// How long informing-only items may wait for a reason to wake the agent before they are delivered anyway.
pub const RIDE_MAX_MS: i64 = 2 * 60_000;

/// Most agents one project may have. A bug or a hostile device registering agents without end would otherwise grow the hub's memory,
/// the chat's channel and every status refresh without limit.
pub const MAX_AGENTS_PER_PROJECT: usize = 500;

/// A delivery that has been sent and not yet accepted, kept to confirm acceptance and measure how long it took.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Pending {
    pub project: String,
    pub agent_id: String,
    pub references: Vec<String>,
    pub task_ids: Vec<String>,
    pub handoffs: Vec<u32>,
    /// What was sent, kept so a session that dies before accepting it can be given it again.
    pub items: Vec<Queued>,
    pub at: i64,
}

use super::tracked::{Cell, Log, TrackedMap, TrackedSet};

pub struct HubCore {
    pub(super) streak_limit: u32,
    pub(super) accept_timeout_ms: u64,
    pub(super) grant_ttl_ms: i64,
    pub(super) conns: HashMap<String, u64>,
    /// When each device last gave a sign of life.
    pub(super) seen_at: HashMap<String, i64>,
    /// What each machine said it can take on. Not saved: a machine says it again every time it connects.
    pub(super) capacity: HashMap<String, NodeCapacity>,
    pub(super) agents: TrackedMap<AgentRow>,
    /// Agent ids per project in the order they joined.
    pub(super) by_project: TrackedMap<Vec<String>>,
    pub(super) status: HashMap<String, (AgentStatus, Option<String>)>,
    /// Owners of the whole workspace, who are owners in every project.
    pub(super) owners: Cell<HashSet<String>>,
    /// Per-project list of humans and their roles.
    pub(super) members: TrackedMap<HashMap<String, Member>>,
    pub(super) asks: TrackedMap<Vec<Ask>>,
    pub(super) perms: TrackedMap<Vec<PermRequest>>,
    pub(super) grants: Cell<Vec<Grant>>,
    pub(super) tasks: TrackedMap<Vec<TaskRow>>,
    /// Agent-only messages in a row, per agent.
    pub(super) streak: HashMap<String, u32>,
    pub(super) queues: TrackedMap<Vec<Queued>>,
    pub(super) pending: TrackedMap<Pending>,
    /// Agents that have already been given their brief.
    pub(super) briefed: TrackedSet,
    /// The latest saved handoff per agent.
    pub(super) handoffs: TrackedMap<Handoff>,
    /// When each agent was last asked to dump its context, so it is not asked over and over.
    pub(super) dump_asked: TrackedMap<i64>,
    /// Agents whose project roster changed since they last heard it.
    pub(super) roster_dirty: TrackedSet,
    pub(super) uploads: HashMap<String, super::files::Upload>,
    pub(super) seq: u64,
    /// The last (epoch, number) taken from each machine, so a frame sent twice is taken once. Saved with everything else.
    pub(super) device_seq: TrackedMap<[u64; 2]>,
    /// The newest chat message (a Discord snowflake) taken from each channel. It is saved in the same commit as the message it belongs to, so
    /// a message read twice (a resumed connection, a read-back after an outage, a restart) is taken once.
    pub(super) chat_last: TrackedMap<u64>,
    /// What has changed since the last save (see `tracked`).
    pub(super) dirty: Log,
    /// The `seq` last written, so it is written again only when it moved.
    pub(super) seq_written: u64,
    pub(super) counters: TrackedMap<u32>,
    pub metrics: Metrics,
}

impl Default for HubCore {
    /// A core with the standard limits.
    fn default() -> Self {
        Self::new(
            DEFAULT_STREAK_LIMIT,
            DEFAULT_ACCEPT_TIMEOUT_MS,
            DEFAULT_GRANT_TTL_MS,
        )
    }
}

impl HubCore {
    /// Builds an empty core. The limits are parameters so tests can make them small.
    pub fn new(streak_limit: u32, accept_timeout_ms: u64, grant_ttl_ms: i64) -> Self {
        let dirty: Log = Default::default();
        Self {
            streak_limit,
            accept_timeout_ms,
            grant_ttl_ms,
            conns: HashMap::new(),
            seen_at: HashMap::new(),
            capacity: HashMap::new(),
            agents: TrackedMap::new("agents", &dirty),
            by_project: TrackedMap::new("by_project", &dirty),
            status: HashMap::new(),
            owners: Cell::new("owners", &dirty, HashSet::new()),
            members: TrackedMap::new("members", &dirty),
            asks: TrackedMap::new("asks", &dirty),
            perms: TrackedMap::new("perms", &dirty),
            grants: Cell::new("grants", &dirty, Vec::new()),
            tasks: TrackedMap::new("tasks", &dirty),
            streak: HashMap::new(),
            queues: TrackedMap::new("queues", &dirty),
            pending: TrackedMap::new("pending", &dirty),
            briefed: TrackedSet::new("briefed", &dirty),
            handoffs: TrackedMap::new("handoffs", &dirty),
            dump_asked: TrackedMap::new("dump_asked", &dirty),
            roster_dirty: TrackedSet::new("roster_dirty", &dirty),
            uploads: HashMap::new(),
            seq: 0,
            seq_written: 0,
            dirty: dirty.clone(),
            counters: TrackedMap::new("counters", &dirty),
            device_seq: TrackedMap::new("device_seq", &dirty),
            chat_last: TrackedMap::new("chat_last", &dirty),
            metrics: Metrics::new(1440),
        }
    }

    // Reading state

    /// The names of every project that has agents.
    pub fn projects(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_project.keys().cloned().collect();
        v.sort();
        v
    }

    /// The projects that have agents on this machine, for telling their chats when something goes wrong with it.
    pub fn projects_of_node(&self, node: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .agents
            .values()
            .filter(|a| a.node_name == node)
            .map(|a| a.project.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The connection a machine has right now, if any.
    pub fn conn_of(&self, node: &str) -> Option<u64> {
        self.conns.get(node).copied()
    }

    /// Whether a numbered frame from a machine is new. A frame whose number is not above the last taken under the same epoch is one the
    /// machine sent again (because it had not seen the ack), and is not taken twice. A new epoch means the machine restarted.
    pub fn accept_seq(&mut self, node: &str, epoch: u64, n: u64) -> bool {
        if self
            .device_seq
            .get(node)
            .is_some_and(|[e, last]| *e == epoch && n <= *last)
        {
            return false;
        }
        self.device_seq.insert(node.to_string(), [epoch, n]);
        true
    }

    /// Whether a chat message is new. Message ids rise with time, so one that is not above the newest taken from its channel has been taken
    /// already. A new one is recorded here, so the record is saved in the same commit as whatever the message causes.
    pub fn take_chat_message(&mut self, channel: &str, id: u64) -> bool {
        if self.chat_last.get(channel).is_some_and(|last| id <= *last) {
            return false;
        }
        self.chat_last.insert(channel.to_string(), id);
        true
    }

    /// Starts watching a channel from `id` (messages above it will be taken, older ones never), unless it is already watched.
    pub fn watch_chat(&mut self, channel: &str, id: u64) {
        if !self.chat_last.contains_key(channel) {
            self.chat_last.insert(channel.to_string(), id);
        }
    }

    /// Every watched channel and the newest message taken from it, for reading back what was said while the hub was away.
    pub fn watched_chats(&self) -> Vec<(String, u64)> {
        self.chat_last
            .iter()
            .map(|(c, id)| (c.clone(), *id))
            .collect()
    }

    /// Looks an agent up by its full id (`project/name`).
    pub fn agent(&self, id: &str) -> Option<&AgentRow> {
        self.agents.get(id)
    }

    /// The agents of a project in the order they joined.
    pub fn agents_of_project(&self, project: &str) -> Vec<&AgentRow> {
        self.by_project
            .get(project)
            .into_iter()
            .flatten()
            .filter_map(|id| self.agents.get(id))
            .collect()
    }

    /// Finds an agent by display name, ignoring case.
    pub fn find_by_name(&self, project: &str, name: &str) -> Option<&AgentRow> {
        let low = name.to_lowercase();
        self.agents_of_project(project)
            .into_iter()
            .find(|a| a.name.to_lowercase() == low)
    }

    /// The agent's last reported status, or offline if it never reported.
    pub fn status_of(&self, agent_id: &str) -> AgentStatus {
        self.status
            .get(agent_id)
            .map_or(AgentStatus::Offline, |s| s.0)
    }

    /// Whether a device is currently connected.
    pub fn is_connected(&self, node: &str) -> bool {
        self.conns.contains_key(node)
    }

    /// Notes that a device gave a sign of life at `now`: a frame, a heartbeat answer, a new connection.
    pub fn touch(&mut self, node: &str, now: i64) {
        self.seen_at.insert(node.to_string(), now);
    }

    /// Every machine the hub knows (connected, or with agents, or seen before), by name.
    pub fn devices(&self) -> Vec<DeviceRow> {
        let mut names: std::collections::BTreeSet<&str> =
            self.conns.keys().map(String::as_str).collect();
        names.extend(self.agents.values().map(|a| a.node_name.as_str()));
        names.extend(self.seen_at.keys().map(String::as_str));
        names
            .into_iter()
            .map(|n| {
                let mut agents: Vec<String> = self
                    .agents
                    .values()
                    .filter(|a| a.node_name == n)
                    .map(|a| a.agent_id.clone())
                    .collect();
                agents.sort();
                DeviceRow {
                    node: n.to_string(),
                    connected: self.conns.contains_key(n),
                    agents,
                    last_seen: self.seen_at.get(n).copied(),
                    max_agents: self.capacity.get(n).map(|c| c.max_agents),
                    labels: self
                        .capacity
                        .get(n)
                        .map(|c| c.labels.clone())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// After the core was rebuilt from a save while a device stayed connected, its agents are presumed alive and idle
    /// (a save does not remember live status). The device's next status report corrects this if it is wrong.
    pub fn assume_online(&mut self, node: &str) {
        let ids: Vec<String> = self
            .agents
            .values()
            .filter(|a| a.node_name == node)
            .map(|a| a.agent_id.clone())
            .collect();
        for id in ids {
            self.status.insert(id, (AgentStatus::Idle, None));
        }
    }

    /// How many devices are connected right now.
    pub fn connected_nodes(&self) -> usize {
        self.conns.len()
    }

    /// Tasks of a project, oldest first.
    pub fn tasks_of(&self, project: &str) -> &[TaskRow] {
        self.tasks.get(project).map_or(&[], Vec::as_slice)
    }

    /// Asks of a project, oldest first, including finished ones.
    pub fn asks_of(&self, project: &str) -> &[Ask] {
        self.asks.get(project).map_or(&[], Vec::as_slice)
    }

    /// Permission requests of a project, oldest first, including finished ones.
    pub fn perms_of(&self, project: &str) -> &[PermRequest] {
        self.perms.get(project).map_or(&[], Vec::as_slice)
    }

    /// Standing permissions still in force at `now`.
    pub fn active_grants(&self, now: i64) -> Vec<&Grant> {
        self.grants.iter().filter(|g| g.expires_at > now).collect()
    }

    /// Puts back tasks saved by an earlier run.
    pub fn restore_tasks(&mut self, rows: Vec<TaskRow>) {
        for t in rows {
            self.tasks.entry(t.project.clone()).or_default().push(t);
        }
        for v in self.tasks.values_mut() {
            v.sort_by_key(|t| t.num);
        }
    }

    // Small helpers shared by the topic files

    /// Next number from a named counter, such as the next Q or P number in a project.
    pub(super) fn next_number(&mut self, key: String) -> u32 {
        let n = self.counters.entry(key).or_insert(0);
        *n += 1;
        *n
    }

    /// The agents of a project that `text` mentions by `@name`, leaving out `exclude`.
    pub(super) fn mentioned(
        &mut self,
        project: &str,
        text: &str,
        exclude: Option<&str>,
    ) -> Vec<AgentRow> {
        self.agents_of_project(project)
            .into_iter()
            .filter(|a| exclude != Some(a.agent_id.as_str()))
            .filter(|a| mentions_name(text, &a.name))
            .cloned()
            .collect()
    }

    /// Sends a frame to the device that owns `a`. Returns false when that device is not connected.
    pub(super) fn send_to(&self, a: &AgentRow, frame: HubFrame, fx: &mut Vec<Effect>) -> bool {
        match self.conns.get(&a.node_name) {
            Some(&conn) => {
                fx.push(Effect::Send { conn, frame });
                true
            }
            None => false,
        }
    }

    /// Adds a line from the system to the project's chat.
    pub(super) fn notice(project: &str, text: String, mention: bool, fx: &mut Vec<Effect>) {
        fx.push(Effect::Chat(Chat::Notice {
            project: project.to_string(),
            text,
            mention,
        }));
    }

    /// Asks the chat to redraw the project's status board.
    pub(super) fn refresh(project: &str, fx: &mut Vec<Effect>) {
        fx.push(Effect::Chat(Chat::RefreshStatus(project.to_string())));
    }

    /// Records something measurable for the dashboard's graphs (see `Persist::Event`).
    pub(super) fn event(
        project: &str,
        kind: &'static str,
        a: &str,
        b: &str,
        n: f64,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        fx.push(Effect::Persist(Persist::Event {
            project: project.into(),
            kind,
            a: a.into(),
            b: b.into(),
            n,
            at: now,
        }));
    }

    /// Records a decision for the audit trail.
    pub(super) fn audit(project: &str, who: &str, what: String, now: i64, fx: &mut Vec<Effect>) {
        fx.push(Effect::Persist(Persist::Audit {
            project: project.into(),
            who: who.into(),
            what,
            at: now,
        }));
    }

    /// Removes credentials from text an agent is about to share, and tells the room when something was removed.
    pub(super) fn scrub(
        &mut self,
        a: &AgentRow,
        text: &str,
        now: i64,
        fx: &mut Vec<Effect>,
    ) -> String {
        let r = redact(text);
        if !r.found.is_empty() {
            self.metrics.inc("redactions", r.found.len() as f64, now);
            let mut kinds: Vec<&str> = Vec::new();
            for k in &r.found {
                if !kinds.contains(&k.as_str()) {
                    kinds.push(k.as_str());
                }
            }
            Self::notice(
                &a.project,
                format!(
                    "Removed {} possible secret(s) ({}) from a message by {}.",
                    r.found.len(),
                    kinds.join(", "),
                    a.name
                ),
                false,
                fx,
            );
        }
        r.text
    }

    // Devices coming and going

    /// A device connected. A second connection for the same device replaces the first, which is told why and closed.
    pub fn node_connected(&mut self, node: &str, conn: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Some(old) = self.conns.insert(node.to_string(), conn) {
            fx.push(Effect::Send {
                conn: old,
                frame: HubFrame::Error {
                    message: "replaced by new connection".into(),
                },
            });
            fx.push(Effect::Close { conn: old });
        }
        fx
    }

    /// A device's connection ended. If it was already replaced, nothing happens. Otherwise its agents go offline.
    pub fn node_disconnected(&mut self, node: &str, conn: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.conns.get(node) != Some(&conn) {
            return fx;
        }
        self.conns.remove(node);
        let mine: Vec<(String, String, String)> = self
            .agents
            .values()
            .filter(|a| a.node_name == node)
            .map(|a| (a.agent_id.clone(), a.project.clone(), a.name.clone()))
            .collect();
        // The moment is the last sign of life: the core reads no clock, and that is as close as it knows.
        let went = self.seen_at.get(node).copied().unwrap_or(0);
        let mut projects: Vec<String> = Vec::new();
        for (id, project, name) in mine {
            if went > 0 {
                Self::event(&project, "status", &name, "offline", 0.0, went, &mut fx);
            }
            self.status.insert(id, (AgentStatus::Offline, None));
            if !projects.contains(&project) {
                projects.push(project);
            }
        }
        for p in projects {
            Self::refresh(&p, &mut fx);
        }
        fx
    }

    // Registering and forgetting agents

    /// Removes an agent from every index. Its queued messages go with it.
    pub(super) fn forget_agent(&mut self, id: &str) {
        if let Some(a) = self.agents.remove(id)
            && let Some(v) = self.by_project.get_mut(&a.project)
        {
            v.retain(|x| x != id);
            if v.is_empty() {
                self.by_project.remove(&a.project);
            }
        }
        self.queues.remove(id);
        self.briefed.remove(id);
        self.roster_dirty.remove(id);
    }

    /// An agent announces itself. Registering again is harmless: nothing is announced and nothing is re-briefed.
    fn register(&mut self, node: &str, spec: &AgentSpec, now: i64, fx: &mut Vec<Effect>) {
        if self
            .agents
            .get(&spec.agent_id)
            .is_some_and(|e| e.node_name != node)
        {
            if let Some(&conn) = self.conns.get(node) {
                let message = format!(
                    "agent {} is already registered by another device",
                    spec.agent_id
                );
                fx.push(Effect::Send {
                    conn,
                    frame: HubFrame::Error { message },
                });
            }
            return;
        }
        if !self.agents.contains_key(&spec.agent_id)
            && self.agents_of_project(&spec.project).len() >= MAX_AGENTS_PER_PROJECT
        {
            if let Some(&conn) = self.conns.get(node) {
                fx.push(Effect::Send {
                    conn,
                    frame: HubFrame::Error {
                        message: format!(
                            "project {} already has {MAX_AGENTS_PER_PROJECT} agents, which is the limit",
                            spec.project
                        ),
                    },
                });
            }
            Self::notice(
                &spec.project,
                format!(
                    "An agent was refused: the project is at its limit of {MAX_AGENTS_PER_PROJECT} agents."
                ),
                false,
                fx,
            );
            return;
        }
        fx.push(Effect::Chat(Chat::EnsureProject(spec.project.clone())));
        let known = self.agents.get(&spec.agent_id).cloned();
        let first = self.agents_of_project(&spec.project).is_empty();
        let is_lead = known.as_ref().map_or(first, |k| k.is_lead);
        let row = AgentRow {
            agent_id: spec.agent_id.clone(),
            name: spec.name.clone(),
            project: spec.project.clone(),
            node_name: node.to_string(),
            adapter: spec.adapter,
            model: spec.model.clone(),
            role: spec.role.clone(),
            is_lead,
        };
        if known.is_none() {
            self.by_project
                .entry(row.project.clone())
                .or_default()
                .push(row.agent_id.clone());
            // Everyone already here learns about the newcomer with their next delivery, at no extra turn.
            for id in self
                .by_project
                .get(&row.project)
                .cloned()
                .unwrap_or_default()
            {
                self.roster_dirty.insert(id);
            }
            self.metrics.inc("agent_joined", 1.0, now);
        }
        self.agents.insert(row.agent_id.clone(), row.clone());
        fx.push(Effect::Persist(Persist::UpsertAgent(row.clone())));
        self.status
            .insert(spec.agent_id.clone(), (AgentStatus::Idle, None));
        Self::event(&row.project, "status", &row.name, "idle", 0.0, now, fx);
        Self::refresh(&row.project, fx);
        self.flush(&row.agent_id, now, fx);
    }

    // Frames from devices

    /// The one entry point for everything a device sends. A device may only act for agents it registered itself.
    pub fn on_node_frame(&mut self, node: &str, frame: NodeFrame, now: i64) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.touch(node, now);
        let claimed = match &frame {
            NodeFrame::AgentStatus { agent_id, .. }
            | NodeFrame::AgentSay { agent_id, .. }
            | NodeFrame::AgentAsk { agent_id, .. }
            | NodeFrame::AgentReport { agent_id, .. }
            | NodeFrame::AgentLimit { agent_id, .. }
            | NodeFrame::AgentGone { agent_id }
            | NodeFrame::AgentAccepted { agent_id, .. }
            | NodeFrame::AgentAssign { agent_id, .. }
            | NodeFrame::AgentTaskDone { agent_id, .. }
            | NodeFrame::AgentPermission { agent_id, .. }
            | NodeFrame::AgentPermissionDone { agent_id, .. }
            | NodeFrame::AgentUsage { agent_id, .. }
            | NodeFrame::AgentHandoff { agent_id, .. }
            | NodeFrame::AgentPickup { agent_id }
            | NodeFrame::AgentTeam { agent_id }
            | NodeFrame::AgentAnswer { agent_id, .. }
            | NodeFrame::FileChunk { agent_id, .. } => Some(agent_id.as_str()),
            NodeFrame::Hello { .. }
            | NodeFrame::AgentRegister { .. }
            | NodeFrame::NodeInfo { .. } => None,
        };
        if claimed
            .and_then(|id| self.agents.get(id))
            .is_some_and(|owner| owner.node_name != node)
        {
            return fx;
        }
        match frame {
            NodeFrame::Hello { .. } => {}
            NodeFrame::NodeInfo {
                cores,
                mem_mb,
                max_agents,
                labels,
            } => {
                self.capacity.insert(
                    node.to_string(),
                    NodeCapacity {
                        cores,
                        mem_mb,
                        max_agents,
                        labels,
                    },
                );
            }
            NodeFrame::AgentRegister { agent, .. } => self.register(node, &agent, now, &mut fx),
            NodeFrame::AgentStatus {
                agent_id,
                status,
                detail,
            } => self.on_status(&agent_id, status, detail, now, &mut fx),
            NodeFrame::AgentSay {
                agent_id,
                text,
                thread,
            } => self.on_say(&agent_id, &text, thread, now, &mut fx),
            NodeFrame::AgentAsk {
                agent_id,
                ask_id,
                question,
                options,
                thread,
            } => self.open_ask(&agent_id, ask_id, &question, options, thread, now, &mut fx),
            NodeFrame::AgentReport {
                agent_id,
                title,
                summary,
                artifacts,
            } => {
                let Some(a) = self.agents.get(&agent_id).cloned() else {
                    return fx;
                };
                let title = self.scrub(&a, &title, now, &mut fx);
                let summary = self.scrub(&a, &summary, now, &mut fx);
                fx.push(Effect::Persist(Persist::History {
                    project: a.project.clone(),
                    thread: None,
                    from: a.name.clone(),
                    kind: "report",
                    // The artifacts go in the record too, so a report read back later (in the vault, say) is whole.
                    text: match artifacts.as_ref().filter(|a| !a.is_empty()) {
                        Some(a) => format!("{title}: {summary}\nArtifacts: {}", a.join(", ")),
                        None => format!("{title}: {summary}"),
                    },
                    at: now,
                }));
                fx.push(Effect::Chat(Chat::Report {
                    project: a.project.clone(),
                    agent: a,
                    title,
                    summary,
                    artifacts,
                }));
            }
            NodeFrame::AgentLimit {
                agent_id,
                kind,
                resets_at,
            } => {
                let Some(a) = self.agents.get(&agent_id).cloned() else {
                    return fx;
                };
                let kind = serde_json::to_value(kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                self.status
                    .insert(agent_id, (AgentStatus::Limited, Some(kind.clone())));
                self.metrics.inc("limits", 1.0, now);
                let when = resets_at
                    .filter(|r| !r.is_empty())
                    .map_or(String::new(), |r| format!(" Resets {r}."));
                Self::notice(
                    &a.project,
                    format!("{} hit a {kind} limit.{when}", a.name),
                    true,
                    &mut fx,
                );
                Self::refresh(&a.project, &mut fx);
            }
            NodeFrame::AgentAccepted { agent_id, msg_ids } => {
                self.on_accepted(&agent_id, &msg_ids, now, &mut fx)
            }
            NodeFrame::AgentAssign {
                agent_id,
                to,
                task,
                thread,
            } => self.assign(&agent_id, &to, &task, thread, now, &mut fx),
            NodeFrame::AgentTaskDone {
                agent_id,
                task_id,
                summary,
            } => self.task_done(&agent_id, &task_id, &summary, now, &mut fx),
            NodeFrame::AgentPermission {
                agent_id,
                perm_id,
                kind,
                action,
                thread,
            } => self.request_permission(&agent_id, perm_id, &kind, &action, thread, now, &mut fx),
            NodeFrame::AgentUsage {
                agent_id,
                kind,
                pct,
            } => self.on_usage(&agent_id, kind, pct, now, &mut fx),
            NodeFrame::AgentHandoff { agent_id, text } => {
                self.on_handoff(&agent_id, &text, now, &mut fx)
            }
            NodeFrame::AgentPickup { agent_id } => self.on_pickup(&agent_id, now, &mut fx),
            NodeFrame::AgentTeam { agent_id } => self.on_team(&agent_id, now, &mut fx),
            NodeFrame::AgentAnswer {
                agent_id,
                ask,
                text,
            } => {
                let Some(a) = self.agents.get(&agent_id).cloned() else {
                    return fx;
                };
                // An agent may answer a peer's question. A refusal (already answered, not allowed) is told to the agent.
                match self.answer_ask(
                    &Answerer::Agent(agent_id.clone()),
                    &a.project,
                    &ask,
                    &text,
                    now,
                ) {
                    Ok(o) => fx.extend(o.effects),
                    Err(Denied::AlreadyDone { by }) => self.tell(
                        &a,
                        format!("{ask} was already answered by {by}."),
                        now,
                        &mut fx,
                    ),
                    Err(_) => self.tell(&a, format!("You cannot answer {ask}."), now, &mut fx),
                }
            }
            NodeFrame::AgentPermissionDone { agent_id, perm_id } => {
                self.permission_done_at_terminal(&agent_id, &perm_id, &mut fx)
            }
            NodeFrame::FileChunk {
                transfer_id,
                agent_id,
                name,
                seq,
                last,
                data,
                to,
                caption,
                thread,
            } => self.on_file_chunk(
                &transfer_id,
                &agent_id,
                &name,
                seq,
                last,
                &data,
                to,
                caption,
                thread,
                now,
                &mut fx,
            ),
            NodeFrame::AgentGone { agent_id } => {
                let Some(a) = self.agents.get(&agent_id).cloned() else {
                    return fx;
                };
                self.forget_agent(&agent_id);
                fx.push(Effect::Persist(Persist::RemoveAgent(agent_id.clone())));
                self.status.remove(&agent_id);
                self.metrics.forget(&agent_id);
                self.cancel_asks_of(&a.project, &agent_id);
                Self::refresh(&a.project, &mut fx);
            }
        }
        fx
    }

    /// An agent reported a new status. Coming out of a hold releases whatever queued up meanwhile.
    fn on_status(
        &mut self,
        agent_id: &str,
        status: AgentStatus,
        detail: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        self.status.insert(agent_id.to_string(), (status, detail));
        self.metrics
            .status(agent_id, super::routing::status_name(status), now);
        Self::event(
            &a.project,
            "status",
            &a.name,
            super::routing::status_name(status),
            0.0,
            now,
            fx,
        );
        Self::refresh(&a.project, fx);
        self.flush(agent_id, now, fx);
    }

    /// An agent said something on purpose: scrub it, keep it, show it, and pass it to the right peers.
    fn on_say(
        &mut self,
        agent_id: &str,
        text: &str,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        self.metrics.inc("msg_agent", 1.0, now);
        let thread = thread.or_else(|| self.open_task_thread(agent_id, &a.project));
        let clean = self.scrub(&a, text, now, fx);
        fx.push(Effect::Persist(Persist::History {
            project: a.project.clone(),
            thread: thread.clone(),
            from: a.name.clone(),
            kind: "say",
            text: clean.clone(),
            at: now,
        }));
        fx.push(Effect::Chat(Chat::Post {
            project: a.project.clone(),
            agent: a.clone(),
            text: clean.clone(),
            thread: thread.clone(),
        }));
        self.route_agent_message(&a, &clean, thread, now, fx);
    }

    /// Whether any word in an action looks like a path the project must never expose.
    pub(super) fn touches_sensitive_path(action: &str) -> bool {
        action
            .split_whitespace()
            .any(|w| is_sensitive_path(w.trim_matches(|c| c == '"' || c == '\'')))
    }

    /// Text that tells an agent about its role and peers. Kept short on purpose, see `briefs`.
    pub(super) fn brief_for(&self, a: &AgentRow) -> String {
        let all = self.agents_of_project(&a.project);
        let peers: Vec<&AgentRow> = all
            .iter()
            .copied()
            .filter(|p| p.agent_id != a.agent_id)
            .collect();
        match all.iter().copied().find(|p| p.is_lead) {
            Some(lead) if lead.agent_id == a.agent_id => briefs::lead(a, &peers),
            Some(lead) => {
                let others: Vec<&AgentRow> = peers
                    .iter()
                    .copied()
                    .filter(|p| p.agent_id != lead.agent_id)
                    .collect();
                briefs::worker(a, lead, &others)
            }
            None => String::new(),
        }
    }
}

/// Whether `text` contains `@name` as a whole word, ignoring case. A word character is a letter, digit or underscore
/// (ASCII, as agent names are), so `@otter` matches in "hi @otter!" but not in "@otters" or "me@otter".
pub(super) fn mentions_name(text: &str, name: &str) -> bool {
    let hay = text.to_ascii_lowercase();
    let needle = format!("@{}", name.to_ascii_lowercase());
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    hay.match_indices(&needle).any(|(i, m)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + m.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}
