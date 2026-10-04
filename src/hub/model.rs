//! The data the hub core works with: who the people and agents are, and the shape of asks, tasks and permissions.
//! Plain structs and enums only. No behaviour lives here, so this file is the quickest way to learn the vocabulary.

use crate::protocol::AdapterId;
use serde::{Deserialize, Serialize};

/// How much a human is trusted in a project. Order matters: a higher role can do everything a lower one can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Role {
    /// Can read everything, instruct nobody.
    Viewer,
    /// Can instruct agents, answer asks, and decide normal-risk permissions.
    Operator,
    /// Can do everything, including granting roles and standing permissions.
    Owner,
}

impl Role {
    /// Lower-case name used in messages shown to agents and humans.
    pub fn name(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Owner => "owner",
        }
    }
}

/// A human the hub knows about, identified by their chat account id (never by what their message says).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub role: Role,
}

/// A person acting right now. Always produced by the edge (chat bridge, web login) from the verified account,
/// never read from message text.
#[derive(Debug, Clone, PartialEq)]
pub struct Human {
    pub id: String,
    pub name: String,
}

/// Who is answering an ask: a person, or another agent (by full agent id).
#[derive(Debug, Clone, PartialEq)]
pub enum Answerer {
    Human(Human),
    Agent(String),
}

/// A file a person attached in chat. The bridge has already downloaded it, checked its size, type and contents, and
/// stored it. The agent is told only a one-line reference, and opens the file with its own tools if it wants to.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// Id in the blob store. The device fetches the bytes by this id.
    pub id: String,
    pub name: String,
    pub size: usize,
    /// Media type as detected from the bytes, not as claimed by the sender.
    pub mime: String,
}

/// Extra facts about a message from a human, supplied by the chat bridge.
#[derive(Debug, Clone, Default)]
pub struct MessageOpts<'a> {
    /// The chat thread it was written in.
    pub thread: Option<&'a str>,
    /// Opaque chat reference, used to mark the message accepted later.
    pub reference: Option<&'a str>,
    /// Set when the message is a reply to an ask (a chat reply, a button, or a thread on the ask). Only then is it
    /// treated as an answer. Without it a message is always an instruction.
    pub answers_ask: Option<&'a str>,
    /// Files attached to the message.
    pub attachments: &'a [Attachment],
}

/// Why the core refused to do something. The caller turns this into a short reply for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// The account is not on the project's list at all. The bridge should ignore the message silently.
    Unlisted,
    /// Known account, but its role is below what the action needs.
    NeedsRole(Role),
    /// Something with that name or id does not exist in this project.
    NotFound,
    /// Someone else already decided or answered, named here.
    AlreadyDone { by: String },
    /// The action is not allowed for this kind of actor (for example an agent approving a permission).
    NotAllowedFor,
}

/// One agent known to the hub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub project: String,
    pub node_name: String,
    pub adapter: AdapterId,
    pub model: Option<String>,
    pub role: Option<String>,
    pub is_lead: bool,
}

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Assigned,
    Accepted,
    Done,
}

/// A unit of work the lead gave to a peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    /// Display id such as T3, unique within a project.
    pub id: String,
    pub project: String,
    pub num: u32,
    pub from_agent: String,
    pub to_agent: String,
    pub text: String,
    pub state: TaskState,
    pub summary: Option<String>,
    pub created: i64,
    pub updated: i64,
}

/// Where an ask is in its life. An ask never disappears because the agent changed status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskState {
    Open,
    Answered { by: String },
    Expired,
    Cancelled,
}

/// A question an agent put to the room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    /// The id the agent's tool gave it.
    pub id: String,
    /// Number shown to people as Q<n>, unique within a project.
    pub qn: u32,
    pub project: String,
    pub agent_id: String,
    pub question: String,
    pub options: Option<Vec<String>>,
    pub thread: Option<String>,
    pub state: AskState,
    pub opened: i64,
    pub reminded: bool,
}

/// How dangerous an action is. Decides who may approve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Normal,
    /// Needs an owner: leaves the project, installs software, pushes code, reaches unknown hosts.
    High,
}

/// Where a permission request is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermState {
    Open,
    Allowed {
        by: String,
        how: String,
    },
    Denied {
        by: String,
    },
    /// Answered at the terminal, so the chat copy is closed.
    Terminal,
    Expired,
}

/// An agent asking a human for permission to do one thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermRequest {
    pub id: String,
    /// Number shown to people as P<n>, unique within a project.
    pub pn: u32,
    pub project: String,
    pub agent_id: String,
    pub kind: String,
    pub action: String,
    pub risk: Risk,
    pub thread: Option<String>,
    pub state: PermState,
    pub opened: i64,
}

/// What a human can answer a permission request with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    /// This request only.
    Once,
    /// Every request of this kind from this agent until the grant expires.
    Kind,
    /// Every request from this agent until the grant expires.
    All,
}

/// A standing permission. Always expires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub project: String,
    /// None means every agent in the project.
    pub agent_id: Option<String>,
    /// None means every kind of action.
    pub kind: Option<String>,
    pub expires_at: i64,
    pub by: String,
    /// Whether the person who granted it was an owner. Only such grants cover high-risk actions.
    pub by_owner: bool,
}

/// What happened to a message a human sent: who got it and who could not take it right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteResult {
    /// Agents the message was queued for.
    pub targets: Vec<String>,
    /// Targets that are not connected, so the message waits.
    pub offline: Vec<String>,
    /// Targets that will not pick the message up right away, with the reason.
    pub held: Vec<(String, &'static str)>,
}

/// Whether a saved handoff is still waiting for a fresh session to carry on from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffState {
    Ready,
    /// A fresh session accepted it. It is never given out again.
    Consumed,
}

/// An agent's saved state, written when its session is about to run out so a fresh session can continue cheaply.
/// Only the latest one per agent is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub agent_id: String,
    pub project: String,
    /// Number of this handoff for the agent, rising with each dump. Stops an old one being mistaken for a new one.
    pub seq: u32,
    pub text: String,
    pub at: i64,
    pub state: HandoffState,
}

/// One machine as the hub sees it: whether it is connected, which agents live on it, and when it last gave a sign of life.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub node: String,
    pub connected: bool,
    pub agents: Vec<String>,
    /// Milliseconds since the Unix epoch of the last frame or heartbeat answer, if it was ever seen.
    pub last_seen: Option<i64>,
    /// How many agents this machine said it should run at once, if it said.
    pub max_agents: Option<u64>,
    /// What kind of machine it said it is.
    pub labels: Vec<String>,
}

/// What a machine said it can take on.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCapacity {
    pub cores: u64,
    pub mem_mb: u64,
    pub max_agents: u64,
    pub labels: Vec<String>,
}
