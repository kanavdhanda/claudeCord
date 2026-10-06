//! The one thing the daemon needs from an agent's terminal, whichever way it is provided: start it, type into it, paste a message
//! safely, look at its screen, notice when it ends, stop it, and let a person get in front of it.
//!
//! There are two ways:
//! - `tmux` (the default wherever tmux is installed: Linux, macOS, and Windows through WSL): each agent is a tmux session. A person
//!   attaches with ordinary tmux and everything they know about tmux works.
//! - the built-in pseudo-terminal (`pty`), for machines without tmux, notably Windows without WSL. claudeCord owns the terminal itself.
//!
//! Neither knows which program is inside. Everything above this file treats them alike.

use super::inject::{Guard, Wait};
use super::pty::PtyTerminal;
use super::tmux::TmuxTerminal;
use std::path::PathBuf;
use std::sync::Arc;

/// Which way terminals are provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// tmux if it is installed, otherwise the built-in terminal.
    Auto,
    /// tmux, on the private server named by the socket.
    Tmux(String),
    /// The built-in pseudo-terminal.
    Pty,
}

impl Backend {
    /// The choice from the environment: `CLAUDECORD_TERMINAL` = `tmux`, `pty` or `auto` (the default).
    pub fn from_env() -> Self {
        match std::env::var("CLAUDECORD_TERMINAL").as_deref() {
            Ok("pty") => Backend::Pty,
            Ok("tmux") => Backend::Tmux(Self::socket_from_env()),
            _ => Backend::Auto,
        }
    }

    /// The name of the private tmux server (`CLAUDECORD_TMUX_SOCKET`, default `claudecord`).
    pub fn socket_from_env() -> String {
        std::env::var("CLAUDECORD_TMUX_SOCKET")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "claudecord".into())
    }

    /// What `Auto` turns into on this machine.
    pub fn resolved(&self) -> Backend {
        match self {
            Backend::Auto if !cfg!(windows) && TmuxTerminal::available() => {
                Backend::Tmux(Self::socket_from_env())
            }
            Backend::Auto => Backend::Pty,
            other => other.clone(),
        }
    }
}

/// What to start and how.
pub struct Spawn<'a> {
    /// A name for the terminal (the agent's id). tmux uses it for the session name.
    pub name: &'a str,
    pub argv: &'a [String],
    pub cwd: PathBuf,
    pub remove_env: &'a [String],
    pub add_env: &'a [(String, String)],
    pub rows: u16,
    pub cols: u16,
}

/// An agent's terminal.
#[derive(Clone)]
pub enum Terminal {
    Tmux(Arc<TmuxTerminal>),
    Pty(Arc<PtyTerminal>),
}

impl Terminal {
    /// Starts a terminal the chosen way.
    pub fn spawn(backend: &Backend, s: &Spawn, guard: Guard) -> std::io::Result<Self> {
        match backend.resolved() {
            Backend::Tmux(socket) => {
                let session: String = format!("cc-{}", s.name)
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                Ok(Terminal::Tmux(Arc::new(TmuxTerminal::spawn(
                    &socket,
                    &session,
                    s.name,
                    s.argv,
                    &s.cwd,
                    s.remove_env,
                    s.add_env,
                    s.rows,
                    s.cols,
                    guard,
                )?)))
            }
            _ => Ok(Terminal::Pty(Arc::new(PtyTerminal::spawn(
                s.argv,
                &s.cwd,
                s.remove_env,
                s.add_env,
                s.rows,
                s.cols,
                guard,
            )?))),
        }
    }

    /// A picture (PNG) of what the terminal shows now, with its colours. Only tmux terminals can give one.
    pub fn picture(&self) -> Option<Vec<u8>> {
        match self {
            Terminal::Tmux(t) => {
                let (ansi, rows, cols) = t.capture_colour()?;
                super::shot::render(&ansi, rows, cols)
            }
            Terminal::Pty(_) => None,
        }
    }

    /// Asks for a look at the next tick (a quiet session is otherwise looked at only every few seconds).
    pub fn hurry(&self) {
        if let Terminal::Tmux(t) = self {
            t.hurry();
        }
    }

    /// Refreshes what is known about the screen. Called once per look by the daemon.
    pub fn observe(&self, now: i64) {
        if let Terminal::Tmux(t) = self {
            t.observe(now);
        }
    }

    /// Types keys as a person would.
    pub fn type_input(&self, bytes: &[u8], now: i64) -> std::io::Result<()> {
        match self {
            Terminal::Tmux(t) => t.type_input(bytes, now),
            Terminal::Pty(t) => t.type_input(bytes, now),
        }
    }

    /// Pastes a message if now is a safe moment, else says what to wait for.
    pub fn inject(&self, text: &str, now: i64) -> Result<(), Wait> {
        match self {
            Terminal::Tmux(t) => t.inject(text, now),
            Terminal::Pty(t) => t.inject(text, now),
        }
    }

    /// The text on the screen.
    pub fn screen_text(&self) -> String {
        match self {
            Terminal::Tmux(t) => t.screen_text(),
            Terminal::Pty(t) => t.screen_text(),
        }
    }

    /// A fingerprint of the screen, to tell whether anything changed.
    pub fn screen_hash(&self) -> u64 {
        match self {
            Terminal::Tmux(t) => t.screen_hash(),
            Terminal::Pty(t) => t.screen_hash(),
        }
    }

    /// Whether the program ended.
    pub fn has_exited(&self) -> bool {
        match self {
            Terminal::Tmux(t) => t.has_exited(),
            Terminal::Pty(t) => t.has_exited(),
        }
    }

    /// Stops the program.
    pub fn kill(&self) {
        match self {
            Terminal::Tmux(t) => t.kill(),
            Terminal::Pty(t) => t.kill(),
        }
    }

    /// Tells the terminal its window changed size.
    pub fn resize(&self, rows: u16, cols: u16) {
        match self {
            Terminal::Tmux(t) => t.resize(rows, cols),
            Terminal::Pty(t) => t.resize(rows, cols),
        }
    }

    /// A command that puts a person in front of the terminal (tmux), or None when claudeCord streams it itself (built-in terminal).
    pub fn attach_command(&self) -> Option<Vec<String>> {
        match self {
            Terminal::Tmux(t) => Some(t.attach_command()),
            Terminal::Pty(_) => None,
        }
    }

    /// The built-in terminal, for streaming it to a window.
    pub fn pty(&self) -> Option<Arc<PtyTerminal>> {
        match self {
            Terminal::Pty(t) => Some(t.clone()),
            Terminal::Tmux(_) => None,
        }
    }
}
