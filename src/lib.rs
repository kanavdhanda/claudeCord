//! claudeCord: run coding agents on any machine and manage them from a chat.
//!
//! This is the Rust port. The TypeScript version it replaces is tagged `ts-reference-v1`, and its behaviour is
//! captured as golden vectors in `testdata/conformance`, which `tests/conformance.rs` checks this code against.

pub mod adapters;
pub mod codes;
pub mod env;
pub mod limits;
pub mod metrics;
pub mod perms;
pub mod protocol;
pub mod redact;
pub mod text;

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
