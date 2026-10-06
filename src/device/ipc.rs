//! How the `claudecord` command talks to the daemon on the same machine: one JSON line each way over a unix socket in
//! the user's claudeCord folder (a unix socket there, or a named pipe on Windows). An agent uses the same door when it runs `claudecord say ...` in its shell, so the
//! verbs an agent has are exactly these requests, with no extra tool definitions loaded into its context.

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, ToFsName, ToNsName,
    tokio::{Listener, Stream as LocalStream},
    traits::tokio::Stream as _,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Choices when starting an agent. Everything here is off unless asked for: nothing happens by itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpOpts {
    /// Run this instead of the agent program's usual command line (anything that runs in a terminal).
    pub command: Option<Vec<String>>,
    /// If another agent here already works in this folder, give this one its own git worktree. Without this such a start is refused.
    pub worktree: bool,
    /// Ask the hub for a saved handoff to carry on from. Without this the agent starts clean.
    pub pickup: bool,
    /// Start the agent again this many times (within ten minutes) if its program ends. Zero means never.
    pub restart: u32,
}

/// What goes over the socket: the request, and the secret key of the agent making it when an agent makes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(flatten)]
    pub req: Req,
}

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
        #[serde(default)]
        opts: UpOpts,
    },
    List,
    Stop {
        agent: String,
    },
    /// Start an agent's program again (every agent here when no name is given), keeping its place in the team.
    Restart {
        agent: Option<String>,
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
    /// Ask who else is in the project.
    Team {
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
    /// The tail of an agent's log (what it was sent and what it did, or what its terminal showed).
    Logs {
        agent: String,
        lines: usize,
        terminal: bool,
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

/// Where the daemon's socket lives (a file; on Windows it only names the pipe).
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

/// The name the daemon listens on: the socket file on unix, a named pipe derived from the folder on Windows.
fn socket_name(dir: &Path) -> std::io::Result<Name<'static>> {
    if cfg!(windows) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        dir.hash(&mut h);
        format!("claudecord-{:x}", h.finish()).to_ns_name::<GenericNamespaced>()
    } else {
        let path = socket_path(dir);
        // A unix socket's path must fit in about 100 bytes (104 on macOS, 108 on Linux): say so, instead of the system's own wording.
        if path.as_os_str().len() >= 100 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "the claudecord folder {} is too long for the daemon's socket (the whole path must stay under 100 characters): set CLAUDECORD_HOME to a shorter folder",
                    dir.display()
                ),
            ));
        }
        path.to_fs_name::<GenericFilePath>()
    }
}

/// Starts listening for local commands, replacing a stale socket left by a daemon that did not shut down cleanly.
pub fn listen(dir: &Path) -> std::io::Result<Listener> {
    ListenerOptions::new()
        .name(socket_name(dir)?)
        .try_overwrite(true)
        .create_tokio()
}

/// Connects to the daemon's socket. Fails if no daemon is listening.
pub async fn connect(dir: &Path) -> std::io::Result<LocalStream> {
    LocalStream::connect(socket_name(dir)?).await
}

/// Sends one request as a person at this machine and reads the one answer. Fails if no daemon is listening.
pub async fn call(dir: &Path, req: &Req) -> std::io::Result<Resp> {
    call_as(dir, None, req).await
}

/// Sends one request, carrying an agent's secret key (agents' own commands need it), and reads the one answer.
pub async fn call_as(dir: &Path, key: Option<&str>, req: &Req) -> std::io::Result<Resp> {
    let mut s = connect(dir).await?;
    let mut line = serde_json::to_string(&Envelope {
        key: key.map(String::from),
        req: req.clone(),
    })
    .expect("plain data");
    line.push('\n');
    s.write_all(line.as_bytes()).await?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).await?;
    serde_json::from_str(&reply).map_err(std::io::Error::other)
}
