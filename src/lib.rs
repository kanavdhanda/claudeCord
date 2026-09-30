//! claudeCord: run coding agents on any machine and manage them from a chat.
//!
//! The expected behaviour of the wire format, secret scrubbing, terminal reading and rate limits is fixed as golden vectors in
//! `testdata/conformance`, which `tests/conformance.rs` checks this code against.

pub mod agents;
pub mod cli;
pub mod device;
pub mod discord;
pub mod export;
pub mod health;
pub mod hub;
pub mod metrics;
pub mod protocol;
pub mod security;
pub mod server;
pub mod store;

// Short paths kept so callers and the conformance tests can say `claudecord::redact` instead of the folder path.
pub use agents::{adapters, text};
pub use discord::perms;
pub use security::{codes, env, limits, redact};

/// Length the way JavaScript counts it, in UTF-16 code units. The wire limits were defined that way, so they are
/// checked that way, or a message with emoji would pass here and fail there.
pub fn jslen(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` in JavaScript terms, without splitting a character.
pub fn jsslice(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// Milliseconds since the Unix epoch. The one place the program reads the clock, so everything else can take the time as
/// an argument and be tested with made-up times.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
