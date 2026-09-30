//! When it is safe to type into an agent's terminal on someone's behalf. A person may be typing in that same terminal,
//! and the agent may be in the middle of something, so a message pasted at the wrong moment would garble the person's
//! half-typed line or land on a prompt. This is pure logic with the time passed in, so every rule can be tested.
//!
//! A message may be pasted only when all of these hold:
//! - the person has not typed for a short while, and has no half-typed line waiting;
//! - the agent has printed nothing for a short while (a busy agent keeps printing);
//! - the agent itself, not some program it started, is the one reading the terminal.

use crate::agents::text::strip_control;

/// Quiet time after the person last typed.
pub const QUIET_INPUT_MS: i64 = 2_000;
/// Quiet time after the agent last printed.
pub const QUIET_OUTPUT_MS: i64 = 3_000;

/// Why a message cannot be pasted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The person typed recently.
    PersonTyping,
    /// The person has a half-typed line.
    PartialLine,
    /// The agent is still printing.
    AgentBusy,
    /// Something other than the agent is reading the terminal, for example a program the agent started.
    NotForeground,
}

/// Watches the terminal's traffic and answers whether a paste is safe.
pub struct Guard {
    quiet_input: i64,
    quiet_output: i64,
    last_input: Option<i64>,
    last_output: i64,
    partial: bool,
}

impl Guard {
    /// A guard with the standard quiet times. `now` is when the agent started, so it counts as just having printed.
    pub fn new(now: i64) -> Self {
        Self::with_times(now, QUIET_INPUT_MS, QUIET_OUTPUT_MS)
    }

    /// A guard with chosen quiet times. Tests use short ones.
    pub fn with_times(now: i64, quiet_input: i64, quiet_output: i64) -> Self {
        Self {
            quiet_input,
            quiet_output,
            last_input: None,
            last_output: now,
            partial: false,
        }
    }

    /// Records bytes the person typed. A line counts as half-typed from the first ordinary key until Enter, Ctrl-C or
    /// Ctrl-U ends or clears it.
    pub fn on_input(&mut self, bytes: &[u8], now: i64) {
        if bytes.is_empty() {
            return;
        }
        self.last_input = Some(now);
        for &b in bytes {
            match b {
                b'\r' | b'\n' | 0x03 | 0x15 => self.partial = false,
                _ => self.partial = true,
            }
        }
    }

    /// Records that the agent printed something.
    pub fn on_output(&mut self, now: i64) {
        self.last_output = now;
    }

    /// Whether a paste is safe right now, or what to wait for. `foreground` says whether the agent itself is the one
    /// reading the terminal.
    pub fn check(&self, now: i64, foreground: bool) -> Result<(), Wait> {
        if !foreground {
            return Err(Wait::NotForeground);
        }
        if self.partial {
            return Err(Wait::PartialLine);
        }
        if self.last_input.is_some_and(|t| now - t < self.quiet_input) {
            return Err(Wait::PersonTyping);
        }
        if now - self.last_output < self.quiet_output {
            return Err(Wait::AgentBusy);
        }
        Ok(())
    }
}

/// The bytes that paste `text` into a terminal as one block and press Enter. Pasting as a block keeps the text from
/// being read as separate keystrokes. Every control character, including the ones that could end the block early, is
/// removed first, so the text cannot escape the paste and be run as typing.
pub fn paste_bytes(text: &str) -> Vec<u8> {
    let mut out = b"\x1b[200~".to_vec();
    out.extend_from_slice(strip_control(text).as_bytes());
    out.extend_from_slice(b"\x1b[201~\r");
    out
}

/// The bytes for a named key, as used by the menu helpers (`Down`, `Up`, `Enter`, `Escape`) or a single character.
pub fn key_bytes(name: &str) -> Vec<u8> {
    match name {
        "Down" => b"\x1b[B".to_vec(),
        "Up" => b"\x1b[A".to_vec(),
        "Enter" => b"\r".to_vec(),
        "Escape" => b"\x1b".to_vec(),
        other => other.as_bytes().to_vec(),
    }
}
