//! How the `claudecord` command talks to the daemon on the same machine: one JSON line each way over a unix socket in
//! the user's claudeCord folder. An agent uses the same door when it runs `claudecord say ...` in its shell, so the
//! verbs an agent has are exactly these requests, with no extra tool definitions loaded into its context.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A request to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    Ping,
    /// Start an agent in a folder.
    Up {
        project: String,
        name: Option<String>,
        adapter: String,
        model: Option<String>,
        role: Option<String>,
        cwd: String,
        policy: String,
        rows: u16,
        cols: u16,
    },
    List,
    Stop {
        agent: String,
    },
    Say {
        agent: String,
        text: String,
        thread: Option<String>,
    },
    Ask {
        agent: String,
        question: String,
        options: Option<Vec<String>>,
        thread: Option<String>,
    },
    Assign {
        agent: String,
        to: String,
        task: String,
        thread: Option<String>,
    },
    Done {
        agent: String,
        task: String,
        summary: String,
    },
    Report {
        agent: String,
        title: String,
        summary: String,
        artifacts: Option<Vec<String>>,
    },
    /// Answer another agent's question.
    Answer {
        agent: String,
        ask: String,
        text: String,
    },
    /// Save this session's state for a fresh session to carry on from.
    Dump {
        agent: String,
        text: String,
    },
    /// Ask for what a fresh session should carry on from.
    Pickup {
        agent: String,
    },
    Usage {
        agent: String,
        kind: String,
        pct: u64,
    },
    Permission {
        agent: String,
        kind: String,
        action: String,
    },
    /// Send a file to the chat, or to a peer if `to` is set.
    Send {
        agent: String,
        path: String,
        to: Option<String>,
        caption: Option<String>,
    },
    /// Switch this connection to a live view of the agent's terminal.
    Attach {
        agent: String,
    },
    Shutdown,
}

/// The daemon's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Resp {
    /// A success with a message.
    pub fn ok(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            msg: msg.into(),
            data: None,
        }
    }

    /// A failure with a reason.
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            msg: msg.into(),
            data: None,
        }
    }
}

/// Where the daemon's socket lives.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

/// Sends one request and reads the one answer. Fails if no daemon is listening.
pub async fn call(dir: &Path, req: &Req) -> std::io::Result<Resp> {
    let mut s = UnixStream::connect(socket_path(dir)).await?;
    let mut line = serde_json::to_string(req).expect("plain data");
    line.push('\n');
    s.write_all(line.as_bytes()).await?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).await?;
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}
