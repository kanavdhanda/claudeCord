//! What a machine writes down about each agent, so that a person (or a fresh session) can pick the work up later. Two files per
//! agent, in `<home>/logs/<agent>/`, readable only by the owner:
//! - `events.jsonl`: one line per thing that happened (what the agent was sent, what it said and asked, permission decisions, status
//!   changes, starts and stops), with the time. Secrets are removed before anything is written.
//! - `terminal.log`: what the agent's terminal showed, as it showed it.
//!
//! Both are trimmed when they grow large, keeping the newest part. `claudecord logs` reads them (secrets removed again, terminal
//! control codes stripped) and `claudecord handoff` turns them into a note for the next session.

use crate::security::redact::redact;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Most a log file may grow to before it is trimmed to its newest half.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// One agent's log folder.
#[derive(Clone)]
pub struct AgentLog {
    dir: PathBuf,
}

impl AgentLog {
    /// The log folder for an agent under the home folder. Created on first write.
    pub fn new(home: &Path, agent_id: &str) -> Self {
        Self {
            dir: home
                .join("logs")
                .join(crate::agents::text::safe_name(&agent_id.replace('/', "_"))),
        }
    }

    /// Where the terminal's output is written.
    pub fn terminal_path(&self) -> PathBuf {
        self.dir.join("terminal.log")
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// Makes the folder now, so something else (tmux) can write into it.
    pub fn prepare(&self) {
        let _ = self.ensure();
    }

    /// Makes the folder (private to the owner) if it is not there yet.
    fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Records one event. The text is scrubbed of secrets and cut to a sensible length. Failing to write is not an error worth
    /// stopping for, so it is ignored.
    pub fn event(&self, at: i64, kind: &str, text: &str) {
        let _ = self.try_event(at, kind, text);
    }

    fn try_event(&self, at: i64, kind: &str, text: &str) -> std::io::Result<()> {
        self.ensure()?;
        let clean: String = redact(text).text.chars().take(2000).collect();
        let line = serde_json::json!({"at": at, "kind": kind, "text": clean}).to_string();
        let path = self.events_path();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")?;
        drop(f);
        trim(&path)
    }

    /// Appends bytes the terminal showed.
    pub fn terminal(&self, bytes: &[u8]) {
        if self.ensure().is_err() {
            return;
        }
        let path = self.terminal_path();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(bytes);
        }
        let _ = trim(&path);
    }

    /// The last `lines` events, oldest first, as readable lines: `time kind: text`.
    pub fn tail_events(&self, lines: usize) -> Vec<String> {
        let text = std::fs::read_to_string(self.events_path()).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .map(|v| {
                format!(
                    "{} {}: {}",
                    v["at"],
                    v["kind"].as_str().unwrap_or("?"),
                    v["text"].as_str().unwrap_or("")
                )
            })
            .collect()
    }

    /// The last `lines` lines the terminal showed, with control codes stripped and secrets removed.
    pub fn tail_terminal(&self, lines: usize) -> Vec<String> {
        let raw = std::fs::read(self.terminal_path()).unwrap_or_default();
        let text = redact(&strip_ansi(&String::from_utf8_lossy(&raw))).text;
        let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|l| l.to_string())
            .collect()
    }
}

/// Cuts a file down to its newest half once it passes the limit, starting at a line boundary.
fn trim(path: &Path) -> std::io::Result<()> {
    let len = std::fs::metadata(path)?.len();
    if len <= MAX_BYTES {
        return Ok(());
    }
    let data = std::fs::read(path)?;
    let from = data.len() - (MAX_BYTES / 2) as usize;
    let start = data[from..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(from, |p| from + p + 1);
    std::fs::write(path, &data[start..])
}

/// Removes terminal control sequences (colours, cursor moves, window titles) and carriage returns, leaving the text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.peek() {
                // CSI: ESC [ ... final byte in @..~
                Some('[') => {
                    it.next();
                    for n in it.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... ended by BEL or ESC \
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' || (n == '\u{1b}' && it.peek() == Some(&'\\')) {
                            if n != '\u{7}' {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // Anything else after ESC is a two-character sequence.
                Some(_) => {
                    it.next();
                }
                None => {}
            },
            '\r' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}
