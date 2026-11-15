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

use super::inject::{Guard, Urgency, Wait};
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
        label: &str,
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
                // The window follows whichever client was used last, so a phone and a desktop terminal sharing it never leave the
                // other with a dotted, cut-off screen.
                "set-option",
                "-g",
                "window-size",
                "latest",
                ";",
                // A program that ends cleanly (Claude's /exit, say) takes its window with it at once. One that fails stays on screen for a moment, so
                // the reason can be read, until the daemon notices and closes it.
                "set-hook",
                "-g",
                "pane-died",
                "if-shell -F \"#{==:#{pane_dead_status},0}\" \"kill-session\"",
                ";",
                // What a person sees at the bottom of an agent's window, and the one key to leave without stopping anything. (This server is
                // claudeCord's own, so none of this touches the person's own tmux.)
                "set-option",
                "-g",
                "status-right-length",
                "70",
                ";",
                "set-option",
                "-g",
                "status-right",
                "Ctrl-] leave (it keeps running) | /exit ends it",
                ";",
                "bind-key",
                "-n",
                "C-]",
                "detach-client",
                ";",
                "bind-key",
                "-n",
                "S-Enter",
                "send-keys",
                "\n",
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
                ";",
                // The agent's own name on the left of its window's status bar (`demo/otter`), so it is clear which one this is.
                "set-option",
                "-t",
                session,
                "status-left",
                &format!("{label} | "),
                ";",
                "set-option",
                "-t",
                session,
                "status-left-length",
                "60",
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
    /// Look at the screen at the next tick, however quiet it has been: something is waiting to be typed into it.
    pub fn hurry(&self) {
        self.seen.locked().next_look = 0;
    }

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

    /// The screen right now WITH its colours (tmux's own rendering of it), with some of what scrolled off above it, and the number of rows and columns. Taken twice, a moment apart, and used only when both
    /// agree, so a screen caught half way through a repaint is not drawn; after three tries the last one is used.
    pub fn capture_colour(&self) -> Option<(String, u16, u16)> {
        let one = || -> Option<(String, u16, u16)> {
            let t = &self.session;
            let size = Self::tmux(
                &self.socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    t,
                    "#{pane_width} #{pane_height}",
                ],
            )
            .ok()?;
            let mut it = size
                .split_whitespace()
                .filter_map(|n| n.parse::<u16>().ok());
            let (w, h) = (it.next()?, it.next()?);
            // The visible screen and up to 300 lines of what scrolled off above it: more of what happened, like a window zoomed out.
            let text = Self::tmux(
                &self.socket,
                &["capture-pane", "-p", "-e", "-S", "-300", "-t", t],
            )
            .ok()?;
            // Empty lines at the bottom are left out, so a mostly empty screen is not a mostly empty picture.
            let text = text.trim_end_matches(['\n', ' ']).to_string();
            let rows = (text.lines().count() as u16).clamp(1, h + 300);
            Some((text, rows, w))
        };
        let mut last = one()?;
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let next = one()?;
            let same = next.0 == last.0;
            last = next;
            if same {
                break;
            }
        }
        Some(last)
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
                ";",
                // `resize-window` pins the size (`manual`); hand it back so attached clients keep resizing it.
                "set-option",
                "-w",
                "-t",
                &self.session,
                "window-size",
                "latest",
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
        self.inject_as(text, now, Urgency::Queue)
    }

    /// Pastes a message as urgently as it asked for.
    pub fn inject_as(&self, text: &str, now: i64, urgency: Urgency) -> Result<(), Wait> {
        // tmux cannot say which program is reading the terminal, so the foreground check always passes here.
        self.guard.locked().check_as(now, true, urgency)?;
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

    /// Returns the private tmux socket and session name for Control Mode (`-CC`) attachment.
    pub fn socket_and_session(&self) -> (&str, &str) {
        (&self.socket, &self.session)
    }

    /// Lists all windows (tabs) in this session, marking the lowest-index window as the primary coding agent tab.
    pub fn list_tabs(&self) -> std::io::Result<Vec<claudecord_protocol::TerminalTab>> {
        let out = Self::tmux(
            &self.socket,
            &[
                "list-windows",
                "-t",
                &self.session,
                "-F",
                "#{window_id}\t#{pane_id}\t#{window_index}\t#{window_name}",
            ],
        )?;
        let mut tabs = Vec::new();
        for (pos, line) in out.lines().enumerate() {
            let mut parts = line.splitn(4, '\t');
            let (Some(win_id), Some(pane_id), Some(idx_s), Some(name)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !claudecord_protocol::is_valid_window_id(win_id)
                || !claudecord_protocol::is_valid_pane_id(pane_id)
            {
                continue;
            }
            let index = idx_s.trim().parse::<u16>().unwrap_or(pos as u16);
            tabs.push(claudecord_protocol::TerminalTab {
                window_id: win_id.to_string(),
                pane_id: pane_id.to_string(),
                index,
                name: if name.trim().is_empty() {
                    "shell".into()
                } else {
                    name.trim().chars().take(60).collect()
                },
                is_agent: pos == 0,
            });
        }
        Ok(tabs)
    }

    /// Captures `pane_id` with ANSI colours and cursor position for initial or tab-switch rendering: the visible screen preceded by
    /// recent scrollback, so the browser has something to scroll through.
    pub fn capture_pane_ansi(&self, pane_id: &str) -> std::io::Result<Vec<u8>> {
        if !claudecord_protocol::is_valid_pane_id(pane_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid pane id",
            ));
        }
        let cursor = Self::tmux(
            &self.socket,
            &[
                "display-message",
                "-p",
                "-t",
                pane_id,
                "#{cursor_x} #{cursor_y}",
            ],
        )
        .unwrap_or_default();
        let mut cx = 0u16;
        let mut cy = 0u16;
        let mut cit = cursor
            .split_whitespace()
            .filter_map(|n| n.parse::<u16>().ok());
        if let (Some(x), Some(y)) = (cit.next(), cit.next()) {
            cx = x;
            cy = y;
        }
        let raw = Self::tmux(
            &self.socket,
            &["capture-pane", "-e", "-p", "-S", "-1500", "-t", pane_id],
        )?;
        let lines: Vec<&str> = raw.split('\n').collect();
        // Trim only trailing newline added by capture-pane -p.
        let slice = if lines.last().is_some_and(|l| l.is_empty()) && lines.len() > 1 {
            &lines[..lines.len() - 1]
        } else {
            &lines[..]
        };
        let mut out = String::from("\x1b[2J\x1b[H");
        out.push_str(&slice.join("\r\n"));
        out.push_str(&format!(
            "\x1b[{};{}H",
            cy.saturating_add(1),
            cx.saturating_add(1)
        ));
        Ok(out.into_bytes())
    }

    /// Opens a new detached shell window (tab) alongside the coding agent in `cwd` with `remove_env` scrubbed and `remain-on-exit off`.
    pub fn open_tab(
        &self,
        cwd: &std::path::Path,
        remove_env: &[String],
    ) -> std::io::Result<claudecord_protocol::TerminalTab> {
        let existing = self.list_tabs()?;
        if existing.len() >= claudecord_protocol::MAX_TERMINAL_TABS {
            return Err(std::io::Error::other(
                "maximum number of terminal tabs reached",
            ));
        }
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = String::from("exec env");
        for n in remove_env {
            cmd.push_str(&format!(" -u {}", quote(n)));
        }
        cmd.push(' ');
        cmd.push_str(&quote(&shell));
        let out = Self::tmux(
            &self.socket,
            &[
                "new-window",
                "-d",
                "-t",
                &self.session,
                "-c",
                &cwd.to_string_lossy(),
                "-P",
                "-F",
                "#{window_id}\t#{pane_id}\t#{window_index}\t#{window_name}",
                &cmd,
            ],
        )?;
        let line = out.lines().next().unwrap_or("");
        let mut parts = line.splitn(4, '\t');
        let (Some(win_id), Some(pane_id), Some(idx_s), Some(name)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(std::io::Error::other("unexpected new-window output"));
        };
        // Secondary shell tabs close cleanly on Ctrl-D / exit without triggering remain-on-exit or killing the agent session.
        let _ = Self::tmux(
            &self.socket,
            &["set-option", "-w", "-t", win_id, "remain-on-exit", "off"],
        );
        Ok(claudecord_protocol::TerminalTab {
            window_id: win_id.to_string(),
            pane_id: pane_id.to_string(),
            index: idx_s.trim().parse::<u16>().unwrap_or(1),
            name: if name.trim().is_empty() {
                "shell".into()
            } else {
                name.trim().chars().take(60).collect()
            },
            is_agent: false,
        })
    }

    /// Closes a secondary shell tab (`pane_id`). Refuses to close the primary coding agent tab (`is_agent`).
    pub fn close_tab(&self, pane_id: &str) -> std::io::Result<()> {
        if !claudecord_protocol::is_valid_pane_id(pane_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid pane id",
            ));
        }
        let tabs = self.list_tabs()?;
        let Some(tab) = tabs.into_iter().find(|t| t.pane_id == pane_id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "tab not found in session",
            ));
        };
        if tab.is_agent {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot close the primary agent tab",
            ));
        }
        Self::tmux(&self.socket, &["kill-window", "-t", &tab.window_id]).map(|_| ())
    }

    /// Sends raw keystroke bytes to `pane_id` using `send-keys -H` (two hex digits per byte, no shell escaping hazards).
    pub fn send_pane_bytes(&self, pane_id: &str, bytes: &[u8], now: i64) -> std::io::Result<()> {
        if !claudecord_protocol::is_valid_pane_id(pane_id)
            || bytes.is_empty()
            || bytes.len() > claudecord_protocol::MAX_TERMINAL_INPUT_BYTES
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid pane input",
            ));
        }
        self.seen.locked().next_look = now;
        self.guard.locked().on_input(bytes, now);
        let hex_args: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut args = vec!["send-keys", "-H", "-t", pane_id];
        for h in &hex_args {
            args.push(h.as_str());
        }
        Self::tmux(&self.socket, &args).map(|_| ())
    }

    /// Resizes the window containing `pane_id` after validating dimensions.
    pub fn resize_pane(&self, pane_id: &str, cols: u16, rows: u16) -> std::io::Result<()> {
        if !claudecord_protocol::is_valid_pane_id(pane_id)
            || !claudecord_protocol::valid_dimensions(cols, rows)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid resize dimensions",
            ));
        }
        let c = cols.to_string();
        let r = rows.to_string();
        Self::tmux(
            &self.socket,
            &[
                "resize-window",
                "-t",
                pane_id,
                "-x",
                &c,
                "-y",
                &r,
                ";",
                "set-option",
                "-w",
                "-t",
                pane_id,
                "window-size",
                "latest",
            ],
        )
        .map(|_| ())
    }
}

/// Lets tmux size every window of `session` from the attached clients again, undoing the fixed size a browser resize set.
pub fn release_window_sizes(socket: &str, session: &str) {
    let Ok(out) = Command::new("tmux")
        .args([
            "-L",
            socket,
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_id}",
        ])
        .stdin(Stdio::null())
        .output()
    else {
        return;
    };
    for w in String::from_utf8_lossy(&out.stdout).lines() {
        let _ = Command::new("tmux")
            .args(["-L", socket, "resize-window", "-A", "-t", w])
            .stdin(Stdio::null())
            .output();
    }
}

/// Stops a whole private tmux server, and every session on it. Used when the daemon shuts down.
pub fn stop_server(socket: &str) {
    let _ = Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .stdin(Stdio::null())
        .output();
}
