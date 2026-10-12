//! What the core asks the outside world to do. The core never performs I/O itself: each call returns a list of these,
//! and the caller (the broker link, the chat bridge, the database writer) carries them out. This is what keeps the
//! core deterministic and cheap to test.

use super::model::{AgentRow, Ask, PermRequest};
use crate::protocol::HubFrame;

/// Something to show in the project's chat. The chat side decides the look; the core decides the content.
#[derive(Debug, Clone, PartialEq)]
pub enum Chat {
    EnsureProject(String),
    /// A deliberate message from an agent, shown under that agent's name, in its thread if it has one.
    Post {
        project: String,
        agent: AgentRow,
        text: String,
        thread: Option<String>,
    },
    /// A question with answer options, to be shown to everyone allowed to answer.
    Ask {
        project: String,
        agent: AgentRow,
        ask: Ask,
    },
    /// A permission request with Deny / Once / Kind / All buttons.
    Permission {
        project: String,
        agent: AgentRow,
        perm: PermRequest,
    },
    /// Close an ask or permission message, saying how it ended ("answered by sam", "resolved in terminal").
    Resolved {
        project: String,
        label: String,
        how: String,
    },
    Report {
        project: String,
        agent: AgentRow,
        title: String,
        summary: String,
        artifacts: Option<Vec<String>>,
    },
    File {
        project: String,
        agent: AgentRow,
        name: String,
        data: Vec<u8>,
        caption: Option<String>,
        thread: Option<String>,
    },
    /// A line from the system itself. `mention` pings the owner.
    Notice {
        project: String,
        text: String,
        mention: bool,
    },
    /// Mark a human message as accepted by an agent, for example with a reaction.
    Confirm {
        project: String,
        reference: String,
        agent_name: String,
    },
    RefreshStatus(String),
    /// A fresh chat for the project: its channel's messages are deleted (the pinned status board stays) and it is said that a new chat has started.
    Clear(String),
}

impl Chat {
    /// The project this is about. Every kind of chat effect belongs to one.
    pub fn project(&self) -> &str {
        match self {
            Chat::EnsureProject(p) | Chat::RefreshStatus(p) | Chat::Clear(p) => p,
            Chat::Post { project, .. }
            | Chat::Ask { project, .. }
            | Chat::Permission { project, .. }
            | Chat::Resolved { project, .. }
            | Chat::Report { project, .. }
            | Chat::File { project, .. }
            | Chat::Notice { project, .. }
            | Chat::Confirm { project, .. } => project,
        }
    }
}

/// Something to save. The core keeps live state in memory, and the store only has to keep up.
#[derive(Debug, Clone, PartialEq)]
pub enum Persist {
    UpsertAgent(AgentRow),
    RemoveAgent(String),
    SetLead {
        project: String,
        agent_id: String,
    },
    /// One line of history: every deliberate message is kept so people can scroll back.
    History {
        project: String,
        thread: Option<String>,
        from: String,
        kind: &'static str,
        text: String,
        at: i64,
    },
    /// A saved handoff, so it survives a hub restart and the device can keep a copy next to the project.
    Handoff {
        project: String,
        agent_id: String,
        seq: u32,
        text: String,
        at: i64,
    },
    /// One measurable thing that happened, for the dashboard's graphs: an agent changing state (`status`), a message reaching an agent
    /// (`edge`, from `a` to `b`), an agent being woken (`turn`, `n` characters delivered), a question being asked (`ask_open`) or
    /// answered (`ask_done`, `n` ms waited), a task changing state (`task`).
    Event {
        project: String,
        kind: &'static str,
        a: String,
        b: String,
        n: f64,
        at: i64,
    },
    /// A decision worth remembering: who allowed or denied what, and when.
    Audit {
        project: String,
        who: String,
        what: String,
        at: i64,
    },
}

/// One thing for the caller to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send {
        conn: u64,
        frame: HubFrame,
    },
    Close {
        conn: u64,
    },
    Chat(Chat),
    Persist(Persist),
    /// After `after_ms`, call `accept_check`, to tell the human if the agent still has not picked a message up.
    AcceptCheck {
        agent_id: String,
        reference: String,
        after_ms: u64,
    },
}
