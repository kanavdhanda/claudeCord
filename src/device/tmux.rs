//! The tmux backend: each agent runs in its own tmux session on a private tmux server (its own socket), so a person can
//! attach with plain `tmux attach`, scroll, and detach as they always do, and the agent keeps running when they leave.
//! claudeCord only ever does four things to a session: start it, type into it, look at its screen, and stop it.
//!
//! Everything works through the `tmux` command, so nothing here knows or cares which program is inside the session. tmux
//! cannot say what a person is typing, only when a client was last active, so the guard here learns of a person typing from
//! that, and cannot know about a half-typed line; messages simply wait for the activity to go quiet.
//!
//! To keep the cost low on machines with many agents, a session is looked at with ONE tmux command (the screen, whether the
//! program ended, and client activity together), and quiet sessions are looked at less and less often.

use super::inject::{Guard, Wait};
use crate::agents::text::strip_control;
use crate::sync::Lock;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// What was last seen of a session.
struct Seen {
    text: String,
    hash: u64,
    dead: bool,
    next_look: i64,
    interval: i64,
}

/// One agent's tmux session.
pub struct TmuxTerminal {
    socket: String,
    session: String,
    guard: Mutex<Guard>,
    seen: Mutex<Seen>,
}

/// Quote a word for a POSIX shell: wrapped in single quotes, with any single quote inside closed, escaped and reopened.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Looks at a quiet session this often at most (milliseconds), and a busy one this often.
const BUSY_EVERY: i64 = 250;
const QUIET_EVERY: i64 = 3000;

impl TmuxTerminal {
    /// Whether a usable tmux is installed.
    pub fn available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Runs one tmux command on this terminal's private server and returns what it printed.
    fn tmux(socket: &str, args: &[&str]) -> std::io::Result<String> {
        let out = Command::new("tmux")
            .args(["-L", socket])
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(std::io::Error::other(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }

    /// Starts `argv` in a new detached session called `session` on tmux server `socket`. Variables in `remove_env` are removed
    /// from the program's environment and `add_env` are set, using `env`, which behaves the same on macOS and Linux.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        socket: &str,
        session: &str,
        argv: &[String],
        cwd: &std::path::Path,
        remove_env: &[String],
        add_env: &[(String, String)],
        rows: u16,
        cols: u16,
        guard: Guard,
    ) -> std::io::Result<Self> {
        let mut cmd = String::from("exec env");
        for n in remove_env {
            cmd.push_str(&format!(" -u {}", quote(n)));
        }
        for (k, v) in add_env {
            cmd.push_str(&format!(" {}", quote(&format!("{k}={v}"))));
        }
        for a in argv {
            cmd.push(' ');
            cmd.push_str(&quote(a));
        }
        let (r, c) = (rows.to_string(), cols.to_string());
        // The option goes first and applies to the whole private server, so a program that ends at once still leaves its
        // last screen behind to be read, instead of the session vanishing.
        Self::tmux(
            socket,
            &[
                "start-server",
                ";",
                "set-option",
                "-g",
                "remain-on-exit",
                "on",
                ";",
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                &c,
                "-y",
                &r,
                "-c",
                &cwd.to_string_lossy(),
                &cmd,
            ],
        )?;
        Ok(Self {
            socket: socket.into(),
            session: session.into(),
            guard: Mutex::new(guard),
            seen: Mutex::new(Seen {
                text: String::new(),
                hash: 0,
                dead: false,
                next_look: 0,
                interval: BUSY_EVERY,
            }),
        })
    }

    /// Has tmux copy everything the session shows into a file, as it shows it.
    pub fn pipe_to(&self, path: &std::path::Path) {
        let target = format!("cat >> {}", quote(&path.to_string_lossy()));
        let _ = Self::tmux(
            &self.socket,
            &["pipe-pane", "-o", "-t", &self.session, &target],
        );
    }

    /// The command that puts a person in front of this session, for the command line to run.
    pub fn attach_command(&self) -> Vec<String> {
        vec![
            "tmux".into(),
            "-L".into(),
            self.socket.clone(),
            "attach-session".into(),
            "-t".into(),
            self.session.clone(),
        ]
    }

    /// Looks at the session now (one tmux command), updating what the guard knows. Quiet sessions are looked at less often.
    pub fn observe(&self, now: i64) {
        let mut seen = self.seen.locked();
        if now < seen.next_look {
            return;
        }
        let t = &self.session;
        let out = Self::tmux(
            &self.socket,
            &[
                "capture-pane",
                "-p",
                "-t",
                t,
                ";",
                "display-message",
                "-p",
                "-t",
                t,
                "CCDEAD#{pane_dead}",
                ";",
                "list-clients",
                "-t",
                t,
                "-F",
                "CCCLIENT#{client_activity}",
            ],
        );
        let Ok(out) = out else {
            // The session is gone (killed, or the whole server stopped).
            seen.dead = true;
            return;
        };
        let mut screen = Vec::new();
        let (mut dead, mut activity) = (false, None);
        for line in out.lines() {
            if let Some(d) = line.strip_prefix("CCDEAD") {
                dead = d.trim() == "1";
            } else if let Some(a) = line.strip_prefix("CCCLIENT") {
                activity = activity.max(a.trim().parse::<i64>().ok());
            } else {
                screen.push(line);
            }
        }
        // Newer tmux versions replace a finished program's screen with a "Pane is dead" line, so keep the last live screen.
        let text = if dead && !seen.text.is_empty() {
            seen.text.clone()
        } else {
            screen.join("\n")
        };
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        let mut guard = self.guard.locked();
        if hash != seen.hash {
            guard.on_output(now);
            seen.interval = BUSY_EVERY;
        } else {
            seen.interval = (seen.interval * 2).min(QUIET_EVERY);
        }
        if let Some(secs) = activity {
            guard.on_activity(secs * 1000);
        }
        seen.text = text;
        seen.hash = hash;
        seen.dead = dead;
        seen.next_look = now + seen.interval;
    }

    /// The screen as of the last look.
    pub fn screen_text(&self) -> String {
        self.seen.locked().text.clone()
    }

    /// A fingerprint of the screen as of the last look.
    pub fn screen_hash(&self) -> u64 {
        self.seen.locked().hash
    }

    /// Whether the program ended, as of the last look.
    pub fn has_exited(&self) -> bool {
        self.seen.locked().dead
    }

    /// Stops the session.
    pub fn kill(&self) {
        let _ = Self::tmux(&self.socket, &["kill-session", "-t", &self.session]);
    }

    /// Tells tmux the window size (matters only when nobody is attached).
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = Self::tmux(
            &self.socket,
            &[
                "resize-window",
                "-t",
                &self.session,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ],
        );
    }

    /// Types keys as a person would: the arrow keys, Enter and Escape by name, everything else as the characters themselves.
    pub fn type_input(&self, bytes: &[u8], now: i64) -> std::io::Result<()> {
        self.seen.locked().next_look = now;
        let mut literal = String::new();
        let flush = |literal: &mut String| -> std::io::Result<()> {
            if !literal.is_empty() {
                Self::tmux(
                    &self.socket,
                    &["send-keys", "-l", "-t", &self.session, "--", literal],
                )?;
                literal.clear();
            }
            Ok(())
        };
        let mut i = 0;
        while i < bytes.len() {
            let key = match &bytes[i..] {
                [0x1b, b'[', b'A', ..] => Some(("Up", 3)),
                [0x1b, b'[', b'B', ..] => Some(("Down", 3)),
                [0x1b, b'[', b'C', ..] => Some(("Right", 3)),
                [0x1b, b'[', b'D', ..] => Some(("Left", 3)),
                [b'\r' | b'\n', ..] => Some(("Enter", 1)),
                [0x1b, ..] => Some(("Escape", 1)),
                [0x03, ..] => Some(("C-c", 1)),
                _ => None,
            };
            match key {
                Some((name, n)) => {
                    flush(&mut literal)?;
                    Self::tmux(&self.socket, &["send-keys", "-t", &self.session, name])?;
                    i += n;
                }
                None => {
                    let end = bytes[i..]
                        .iter()
                        .position(|b| *b < 0x20)
                        .map_or(bytes.len(), |p| i + p)
                        .max(i + 1);
                    literal.push_str(&String::from_utf8_lossy(&bytes[i..end]));
                    i = end;
                }
            }
        }
        flush(&mut literal)
    }

    /// Pastes a message into the session as one block and presses Enter, if the guard says now is safe. The text has every
    /// control character removed first, so it cannot escape the paste.
    pub fn inject(&self, text: &str, now: i64) -> Result<(), Wait> {
        // tmux cannot say which program is reading the terminal, so the foreground check always passes here.
        self.guard.locked().check(now, true)?;
        let clean = strip_control(text);
        let buffer = format!("cc-{}", self.session);
        let loaded = (|| -> std::io::Result<()> {
            let mut child = Command::new("tmux")
                .args(["-L", &self.socket, "load-buffer", "-b", &buffer, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(clean.as_bytes())?;
            child.wait()?;
            Self::tmux(
                &self.socket,
                &[
                    "paste-buffer",
                    "-p",
                    "-d",
                    "-b",
                    &buffer,
                    "-t",
                    &self.session,
                ],
            )?;
            // Give a real UI time to finish taking the paste, or its Enter can be swallowed as part of it.
            // ponytail: blocks this thread 150 ms per paste; make it async if many agents get messages at once.
            std::thread::sleep(std::time::Duration::from_millis(150));
            Self::tmux(&self.socket, &["send-keys", "-t", &self.session, "Enter"])?;
            Ok(())
        })();
        self.seen.locked().next_look = now;
        // A failed paste means the session is gone, which the next look will show. The caller just tries again later.
        loaded.map_err(|_| Wait::AgentBusy)
    }
}

/// Stops a whole private tmux server, and every session on it. Used when the daemon shuts down.
pub fn stop_server(socket: &str) {
    let _ = Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .stdin(Stdio::null())
        .output();
}
