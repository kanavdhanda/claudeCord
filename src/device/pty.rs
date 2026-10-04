//! One agent's terminal. The device starts the agent inside a pseudo-terminal that it owns, so the agent keeps running
//! when the person's own terminal window closes, and a window can attach later. This file starts and stops the program,
//! watches what it prints (keeping a picture of the screen), and carries keystrokes in: the person's, or a message
//! from the hub pasted at a safe moment (see `inject`).

use super::inject::{Guard, Wait, paste_bytes};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// A running agent.
pub struct AgentProc {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    screen: Arc<Mutex<vt100::Parser>>,
    guard: Arc<Mutex<Guard>>,
    exited: Arc<AtomicBool>,
    /// Everything the agent prints, for windows that are attached.
    pub output: broadcast::Sender<Vec<u8>>,
}

impl AgentProc {
    /// Starts `argv` in `cwd` inside a new terminal of the given size. Variables named in `remove_env` are taken out of
    /// the agent's environment (secrets), and `add_env` are set. The guard decides when pasting is safe.
    pub fn spawn(
        argv: &[String],
        cwd: &Path,
        remove_env: &[String],
        add_env: &[(String, String)],
        rows: u16,
        cols: u16,
        guard: Guard,
    ) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(std::io::Error::other)?;
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.cwd(cwd);
        for name in remove_env {
            cmd.env_remove(name);
        }
        for (k, v) in add_env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let guard = Arc::new(Mutex::new(guard));
        let exited = Arc::new(AtomicBool::new(false));
        let (output, _) = broadcast::channel(256);
        {
            let (screen, guard, exited, output) = (
                screen.clone(),
                guard.clone(),
                exited.clone(),
                output.clone(),
            );
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    screen.lock().expect("lock").process(&buf[..n]);
                    guard.lock().expect("lock").on_output(crate::now_ms());
                    let _ = output.send(buf[..n].to_vec());
                }
                exited.store(true, Ordering::SeqCst);
            });
        }
        Ok(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            screen,
            guard,
            exited,
            output,
        })
    }

    /// Keys typed by a person in an attached window. Written straight through, and noted by the guard.
    pub fn type_input(&self, bytes: &[u8], now: i64) -> std::io::Result<()> {
        self.guard.lock().expect("lock").on_input(bytes, now);
        let mut w = self.writer.lock().expect("lock");
        w.write_all(bytes)?;
        w.flush()
    }

    /// A screen-sized fingerprint, to tell whether anything on the screen changed.
    pub fn screen_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.screen_text().hash(&mut h);
        h.finish()
    }

    /// The bytes that redraw the current screen in a freshly attached window.
    pub fn redraw_bytes(&self) -> Vec<u8> {
        self.screen
            .lock()
            .expect("lock")
            .screen()
            .contents_formatted()
    }

    /// Whether the agent itself (and not a program it started) is the one reading the terminal.
    fn foreground(&self) -> bool {
        let leader = self.master.lock().expect("lock").process_group_leader();
        let pid = self
            .child
            .lock()
            .expect("lock")
            .process_id()
            .map(|p| p as i32);
        match (leader, pid) {
            (Some(l), Some(p)) => l == p,
            // If the system cannot say, assume it is the agent rather than never delivering.
            _ => true,
        }
    }

    /// Pastes a message into the agent's terminal if that is safe now. Returns what to wait for otherwise.
    pub fn inject(&self, text: &str, now: i64) -> Result<(), Wait> {
        self.guard
            .lock()
            .expect("lock")
            .check(now, self.foreground())?;
        let mut w = self.writer.lock().expect("lock");
        let _ = w.write_all(&paste_bytes(text)).and_then(|_| w.flush());
        Ok(())
    }

    /// The text currently on the agent's screen.
    pub fn screen_text(&self) -> String {
        self.screen.lock().expect("lock").screen().contents()
    }

    /// Tells the terminal its window changed size.
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.lock().expect("lock").resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.screen
            .lock()
            .expect("lock")
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Whether the program has ended.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Stops the program.
    pub fn kill(&self) {
        let _ = self.child.lock().expect("lock").kill();
    }
}
