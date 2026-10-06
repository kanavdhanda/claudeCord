//! The frames a device and the hub exchange, and the rules a frame must meet to be accepted.
//!
//! Frames come from machines that may be compromised, so everything is validated here once, at the edge: names,
//! lengths and counts. A frame that is wrong in any way is dropped as a whole.

use crate::jslen;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Raw bytes per file chunk. Base64 on the wire makes each frame about a third larger.
pub const FILE_CHUNK_BYTES: usize = 192 * 1024;
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const NODE_CONNECT_PATH: &str = "/api/v1/node/connect";

static SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$").unwrap());
static MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/@+\-\[\]]{0,79}$").unwrap());
static NO_CONTROL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\x00-\x1f\x7f]*$").unwrap());

/// Names end up in Discord channel names, tmux titles, file paths and prompts, so they are checked once, here.
pub fn is_slug(s: &str) -> bool {
    SLUG.is_match(s)
}

/// Why `name` cannot be an agent's name, or None if it can. An agent posts to Discord under its own name and is @mentioned by it, so on top of the
/// usual letters, digits, dots, dashes and underscores: Discord refuses webhook names that contain "discord" or "clyde", `@everyone` and `@here` already
/// ping the whole server, and a long name is cut off in the member list. (Names are used in lower case, so `Otter` and `otter` are the same agent.)
pub fn agent_name_problem(name: &str) -> Option<&'static str> {
    let l = name.to_ascii_lowercase();
    if !is_slug(name) {
        Some("names use letters, digits, dots, dashes and underscores")
    } else if name.len() > 32 {
        Some("an agent's name is at most 32 characters")
    } else if l.contains("discord") || l.contains("clyde") {
        Some("Discord does not accept names that contain \"discord\" or \"clyde\"")
    } else if l == "everyone" || l == "here" {
        Some(
            "@everyone and @here already mean something in Discord, so an agent cannot be called that",
        )
    } else {
        None
    }
}

/// Deserialises an optional field strictly: absent is fine, but an explicit `null` is not.
mod opt {
    use serde::{Deserialize, Deserializer};
    /// Reads an optional field but rejects an explicit null, as the wire format has always required.
    pub fn de<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
        T::deserialize(d).map(Some)
    }
}

/// A whole number that is not negative. JSON has one number type, so `1.0` counts, as it did before.
fn uint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = f64::deserialize(d)?;
    if v.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&v) {
        Ok(v as u64)
    } else {
        Err(serde::de::Error::custom("expected a non-negative integer"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Idle,
    Thinking,
    Executing,
    WaitingInput,
    Paused,
    Limited,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterId {
    Claude,
    Agy,
    Codex,
}

impl AdapterId {
    /// The adapter name as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitKind {
    Session,
    Weekly,
    Rate,
}

/// What a usage reading measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageKind {
    /// How full this agent's own context window is.
    Context,
    /// How much of the account's session allowance is used. Shared by every agent on the account.
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    pub agent_id: String,
    pub name: String,
    pub project: String,
    pub adapter: AdapterId,
    #[serde(
        default,
        deserialize_with = "opt::de",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt::de",
        skip_serializing_if = "Option::is_none"
    )]
    pub role: Option<String>,
}

impl AgentSpec {
    /// Whether the value meets its size and format limits.
    pub fn is_valid(&self) -> bool {
        jslen(&self.agent_id) <= 200
            && is_slug(&self.name)
            && is_slug(&self.project)
            && self.model.as_deref().is_none_or(|m| MODEL.is_match(m))
            && self
                .role
                .as_deref()
                .is_none_or(|r| jslen(r) <= 120 && NO_CONTROL.is_match(r))
            && self.agent_id == format!("{}/{}", self.project, self.name)
    }
}

/// Frames from a device to the hub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum NodeFrame {
    #[serde(rename = "hello", rename_all = "camelCase")]
    Hello { node_name: String, version: String },
    #[serde(rename = "agent.register")]
    AgentRegister { agent: AgentSpec, cwd: String },
    #[serde(rename = "agent.status", rename_all = "camelCase")]
    AgentStatus {
        agent_id: String,
        status: AgentStatus,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        detail: Option<String>,
    },
    #[serde(rename = "agent.say", rename_all = "camelCase")]
    AgentSay {
        agent_id: String,
        text: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
    #[serde(rename = "agent.ask", rename_all = "camelCase")]
    AgentAsk {
        agent_id: String,
        ask_id: String,
        question: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        options: Option<Vec<String>>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
    #[serde(rename = "agent.report", rename_all = "camelCase")]
    AgentReport {
        agent_id: String,
        title: String,
        summary: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        artifacts: Option<Vec<String>>,
    },
    #[serde(rename = "agent.limit", rename_all = "camelCase")]
    AgentLimit {
        agent_id: String,
        kind: LimitKind,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        resets_at: Option<String>,
    },
    #[serde(rename = "agent.gone", rename_all = "camelCase")]
    AgentGone { agent_id: String },
    /// What an agent's terminal shows right now, with why it is being sent (it was asked for, it finished without answering, it looks stuck).
    #[serde(rename = "agent.screen", rename_all = "camelCase")]
    AgentScreen {
        agent_id: String,
        why: String,
        text: String,
    },
    /// Sent by a machine each time it connects, after its registrations: every agent it is running, and so, by leaving one out, which of its
    /// agents the hub still lists are gone.
    #[serde(rename = "agents.here", rename_all = "camelCase")]
    AgentsHere { agent_ids: Vec<String> },
    #[serde(rename = "agent.accepted", rename_all = "camelCase")]
    AgentAccepted {
        agent_id: String,
        msg_ids: Vec<String>,
    },
    #[serde(rename = "agent.assign", rename_all = "camelCase")]
    AgentAssign {
        agent_id: String,
        to: String,
        task: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
    #[serde(rename = "agent.taskdone", rename_all = "camelCase")]
    AgentTaskDone {
        agent_id: String,
        task_id: String,
        summary: String,
    },
    /// The harness wants to do something that needs a human decision (run a command, edit a file, reach the network).
    #[serde(rename = "agent.permission", rename_all = "camelCase")]
    AgentPermission {
        agent_id: String,
        perm_id: String,
        /// Broad class of action, for example `bash`, `edit`, `net`. Grants match on this.
        kind: String,
        /// What exactly it wants to do, shown to the human as is.
        action: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
    /// What this machine can take on, sent each time it connects. The hub uses it to place new agents sensibly.
    #[serde(rename = "node.info", rename_all = "camelCase")]
    NodeInfo {
        #[serde(deserialize_with = "uint")]
        cores: u64,
        #[serde(deserialize_with = "uint")]
        mem_mb: u64,
        /// Most agents this machine should run at once.
        #[serde(deserialize_with = "uint")]
        max_agents: u64,
        /// What kind of machine it is, for example `gpu` or `h100`. A request can ask for a label.
        labels: Vec<String>,
    },
    /// A usage reading from the harness, as a percentage. At 97 the hub asks for a handoff.
    #[serde(rename = "agent.usage", rename_all = "camelCase")]
    AgentUsage {
        agent_id: String,
        kind: UsageKind,
        #[serde(deserialize_with = "uint")]
        pct: u64,
    },
    /// The agent's saved state, written so a fresh session can carry on (the `context_dump` verb).
    #[serde(rename = "agent.handoff", rename_all = "camelCase")]
    AgentHandoff { agent_id: String, text: String },
    /// An agent answers another agent's question (never a permission request).
    #[serde(rename = "agent.answer", rename_all = "camelCase")]
    AgentAnswer {
        agent_id: String,
        ask: String,
        text: String,
    },
    /// A fresh session asking for whatever it should carry on from (the `pickup` verb).
    #[serde(rename = "agent.pickup", rename_all = "camelCase")]
    AgentPickup { agent_id: String },
    /// An agent asking who else is in its project and whether they can be reached now (the `team` verb).
    #[serde(rename = "agent.team", rename_all = "camelCase")]
    AgentTeam { agent_id: String },
    /// A machine could not start an agent the hub asked it to, so the people who asked can be told why.
    #[serde(rename = "spawn.failed", rename_all = "camelCase")]
    SpawnFailed {
        project: String,
        name: String,
        reason: String,
    },
    /// The same prompt was answered at the terminal instead, so the chat message can be closed.
    #[serde(rename = "agent.permission.done", rename_all = "camelCase")]
    AgentPermissionDone { agent_id: String, perm_id: String },
    #[serde(rename = "file.chunk", rename_all = "camelCase")]
    FileChunk {
        transfer_id: String,
        agent_id: String,
        name: String,
        #[serde(deserialize_with = "uint")]
        seq: u64,
        last: bool,
        data: String,
        /// SHA-256 of the whole file (hex), on the last piece: the receiver checks what it put together against it.
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        sha256: Option<String>,
        /// Peer agent to deliver to. Absent means post to the channel.
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        to: Option<String>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        caption: Option<String>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
}

/// Whether a string is at most `max` characters, counted the way JavaScript counts.
fn within(s: &str, max: usize) -> bool {
    jslen(s) <= max
}

/// Same as `within`, and an absent value passes.
fn within_opt(s: &Option<String>, max: usize) -> bool {
    s.as_deref().is_none_or(|s| jslen(s) <= max)
}

impl NodeFrame {
    /// Whether the frame meets its size and count limits. Types and required fields were checked by parsing.
    pub fn is_valid(&self) -> bool {
        use NodeFrame::*;
        match self {
            Hello { .. } | AgentStatus { .. } | AgentGone { .. } => true,
            AgentRegister { agent, .. } => agent.is_valid(),
            // No limit of our own below the frame size; Discord's own limit is handled when posting (see `split_for_discord`).
            AgentSay { text, thread, .. } => within(text, 200_000) && within_opt(thread, 90),
            AgentAsk {
                ask_id,
                question,
                options,
                thread,
                ..
            } => {
                within(ask_id, 300)
                    && within(question, 4000)
                    && options
                        .as_ref()
                        .is_none_or(|o| o.len() <= 12 && o.iter().all(|s| within(s, 200)))
                    && within_opt(thread, 90)
            }
            AgentReport {
                title,
                summary,
                artifacts,
                ..
            } => {
                within(title, 200)
                    && within(summary, 8000)
                    && artifacts
                        .as_ref()
                        .is_none_or(|a| a.len() <= 20 && a.iter().all(|s| within(s, 500)))
            }
            AgentLimit { .. } => true,
            AgentAccepted { msg_ids, .. } => {
                msg_ids.len() <= 200 && msg_ids.iter().all(|s| within(s, 40))
            }
            AgentAssign {
                to, task, thread, ..
            } => within(to, 64) && within(task, 4000) && within_opt(thread, 90),
            AgentTaskDone {
                task_id, summary, ..
            } => within(task_id, 20) && within(summary, 4000),
            AgentPermission {
                perm_id,
                kind,
                action,
                thread,
                ..
            } => {
                within(perm_id, 300)
                    && within(kind, 40)
                    && within(action, 2000)
                    && within_opt(thread, 90)
            }
            AgentPermissionDone { perm_id, .. } => within(perm_id, 300),
            AgentUsage { pct, .. } => *pct <= 100,
            NodeInfo {
                max_agents, labels, ..
            } => *max_agents <= 1000 && labels.len() <= 16 && labels.iter().all(|l| within(l, 32)),
            AgentHandoff { text, .. } => within(text, 8000),
            AgentAnswer { ask, text, .. } => within(ask, 300) && within(text, 4000),
            AgentPickup { .. } | AgentTeam { .. } => true,
            AgentScreen { why, text, .. } => within(why, 200) && within(text, 4000),
            AgentsHere { agent_ids } => {
                agent_ids.len() <= 1000 && agent_ids.iter().all(|i| within(i, 200))
            }
            SpawnFailed {
                project,
                name,
                reason,
            } => is_slug(project) && is_slug(name) && within(reason, 300),
            FileChunk {
                transfer_id,
                name,
                seq,
                data,
                sha256,
                to,
                caption,
                thread,
                ..
            } => {
                within(transfer_id, 80)
                    && within(name, 255)
                    && *seq <= 1000
                    && within(data, (FILE_CHUNK_BYTES * 4).div_ceil(3) + 16)
                    && sha256
                        .as_deref()
                        .is_none_or(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                    && within_opt(to, 64)
                    && within_opt(caption, 1000)
                    && within_opt(thread, 90)
            }
        }
    }

    /// Parses and validates one frame. Anything wrong at all gives `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str::<Self>(raw)
            .ok()
            .filter(Self::is_valid)
    }
}

/// Frames from the hub to a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum HubFrame {
    #[serde(rename = "welcome", rename_all = "camelCase")]
    Welcome { node_id: String },
    /// Every frame a machine numbered (see `stamp_of`) up to and including `n` is on disk and will not be asked for again. Sent only
    /// after the change the frame caused is saved, so a machine that has the ack can forget the frame for good.
    #[serde(rename = "ack")]
    Ack { n: u64 },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "deliver", rename_all = "camelCase")]
    Deliver {
        agent_id: String,
        from: String,
        text: String,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
        /// Echoed back in `agent.accepted` once the agent starts on this delivery.
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        msg_id: Option<String>,
    },
    #[serde(rename = "answer", rename_all = "camelCase")]
    Answer {
        agent_id: String,
        ask_id: String,
        text: String,
    },
    /// Text typed into the agent's terminal exactly as written, with no sender header. This is how a person sends the
    /// harness's own commands (`/compact`, `@file` and so on) without claudeCord knowing what any of them mean.
    #[serde(rename = "raw", rename_all = "camelCase")]
    Raw { agent_id: String, text: String },
    /// A human's decision on a permission request.
    #[serde(rename = "decision", rename_all = "camelCase")]
    Decision {
        agent_id: String,
        perm_id: String,
        allow: bool,
    },
    #[serde(rename = "file.chunk", rename_all = "camelCase")]
    FileChunk {
        transfer_id: String,
        agent_id: String,
        from: String,
        name: String,
        #[serde(deserialize_with = "uint")]
        seq: u64,
        last: bool,
        data: String,
        /// SHA-256 of the whole file (hex), on the last piece: the receiver checks what it put together against it.
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        sha256: Option<String>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        caption: Option<String>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        thread: Option<String>,
    },
    /// The hub never chooses a directory. Devices only start agents in folders they registered themselves.
    #[serde(rename = "spawn")]
    Spawn { agent: AgentSpec },
    #[serde(rename = "stop", rename_all = "camelCase")]
    Stop { agent_id: String },
    /// Start this agent's program again from its base: a new session in a new terminal, same name and folder.
    #[serde(rename = "restart", rename_all = "camelCase")]
    Restart { agent_id: String },
    /// Send back what this agent's terminal shows now.
    Screen { agent_id: String },
    #[serde(rename = "killall")]
    Killall {
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        project: Option<String>,
    },
    #[serde(rename = "hold", rename_all = "camelCase")]
    Hold {
        on: bool,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        agent_id: Option<String>,
        #[serde(
            default,
            deserialize_with = "opt::de",
            skip_serializing_if = "Option::is_none"
        )]
        project: Option<String>,
    },
}

/// The epoch and sequence number a machine put on a frame, if it did. A machine numbers what it sends (1, 2, 3...) under an epoch chosen
/// when it starts, keeps each frame until the hub acknowledges it, and sends the unacknowledged ones again after a reconnect. The hub
/// remembers the last number it took per machine, so a frame sent twice is taken once but acknowledged both times.
pub fn stamp_of(raw: &str) -> Option<(u64, u64)> {
    #[derive(Deserialize)]
    struct Stamp {
        e: Option<u64>,
        n: Option<u64>,
    }
    let s: Stamp = serde_json::from_str(raw).ok()?;
    Some((s.e?, s.n?))
}

impl HubFrame {
    /// Whether the value meets its size and format limits.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Spawn { agent } => agent.is_valid(),
            _ => true,
        }
    }

    /// Parses and validates one frame. Anything wrong at all gives `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str::<Self>(raw)
            .ok()
            .filter(Self::is_valid)
    }
}

const ADJECTIVES: [&str; 8] = [
    "quick", "calm", "bright", "steady", "keen", "bold", "wry", "sharp",
];
const NOUNS: [&str; 8] = [
    "otter", "heron", "lynx", "falcon", "badger", "finch", "marten", "ibis",
];

/// A friendly name for an agent that was not given one, avoiding the names already in use.
pub fn auto_name(
    taken: &std::collections::HashSet<String>,
    mut rand: impl FnMut(usize) -> usize,
) -> String {
    for _ in 0..50 {
        let n = format!(
            "{}-{}",
            ADJECTIVES[rand(ADJECTIVES.len())],
            NOUNS[rand(NOUNS.len())]
        );
        if !taken.contains(&n) {
            return n;
        }
    }
    format!("agent-{:04x}", rand(0x10000))
}
