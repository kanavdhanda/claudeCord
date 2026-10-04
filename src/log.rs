//! Logging: one line per event, `2026-10-04 14:05:09 UTC INFO  hub: message`, to stderr and, once a file is set, to a size-capped
//! file as well. The level comes from `CLAUDECORD_LOG` (`error`, `warn`, `info` (default) or `debug`). Every line is passed through
//! the same secret scrubbing as everything else that leaves the program, so a token that ends up in a message never reaches a log.
//! A panic anywhere is logged with where it happened (and the program carries on wherever the code is built to survive it).
//!
//! This is small on purpose: no logging framework, no extra crates. Use the macros: `info!("hub", "listening on {addr}")`.
//! Log the things a person debugging at 3 a.m. needs (starts, stops, connections made and lost with the reason, refusals, errors),
//! never one line per message, and never message text.

use crate::sync::Lock;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
        }
    }
}

/// The file being logged to, and how much it holds.
struct Sink {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

/// A log file is moved aside to `NAME.1` (replacing the older one) when it passes this size, so logs never fill a small disk.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// Sets the level from the environment, installs the panic hook, and starts logging to `file` if given (made private: logs can hold
/// names and addresses). Safe to call more than once; the last file wins.
pub fn init(file: Option<PathBuf>) {
    if let Some(l) = std::env::var("CLAUDECORD_LOG")
        .ok()
        .and_then(|v| Level::parse(&v))
    {
        LEVEL.store(l as u8, Ordering::Relaxed);
    }
    if let Some(path) = file {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(f) => {
                let bytes = f.metadata().map_or(0, |m| m.len());
                *sink().locked() = Some(Sink {
                    path,
                    file: f,
                    bytes,
                });
            }
            Err(e) => eprintln!("could not open the log file {}: {e}", path.display()),
        }
    }
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let at = info
                .location()
                .map_or("?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
            let what = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            let thread = std::thread::current();
            emit(
                Level::Error,
                "panic",
                format_args!(
                    "{what} at {at} in thread {}",
                    thread.name().unwrap_or("unnamed")
                ),
            );
        }));
    });
}

/// Whether a line at this level would be written.
pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

/// Writes one line. Used through the macros.
pub fn emit(level: Level, target: &str, msg: std::fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let text = crate::security::redact::redact(&msg.to_string()).text;
    // One event is one line: anything that would break the line is flattened.
    let text = text.replace(['\n', '\r'], " ");
    let line = format!(
        "{} {} {target}: {text}\n",
        crate::uptime::iso(crate::now_ms()),
        level.name()
    );
    // `eprint!` rather than writing to stderr directly, so a test run keeps the lines of a passing test out of its output.
    eprint!("{line}");
    let mut guard = sink().locked();
    if let Some(s) = guard.as_mut() {
        if s.bytes + line.len() as u64 > MAX_FILE_BYTES {
            let old = s.path.with_extension("log.1");
            let _ = std::fs::rename(&s.path, old);
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&s.path)
            {
                s.file = f;
                s.bytes = 0;
            }
        }
        if s.file.write_all(line.as_bytes()).is_ok() {
            s.bytes += line.len() as u64;
        }
    }
}

#[macro_export]
macro_rules! error { ($t:expr, $($a:tt)*) => { $crate::log::emit($crate::log::Level::Error, $t, format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($t:expr, $($a:tt)*) => { $crate::log::emit($crate::log::Level::Warn, $t, format_args!($($a)*)) } }
#[macro_export]
macro_rules! info { ($t:expr, $($a:tt)*) => { $crate::log::emit($crate::log::Level::Info, $t, format_args!($($a)*)) } }
#[macro_export]
macro_rules! debug { ($t:expr, $($a:tt)*) => { $crate::log::emit($crate::log::Level::Debug, $t, format_args!($($a)*)) } }
