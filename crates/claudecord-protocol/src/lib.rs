//! Wire protocol types, framing, and text sanitization for claudeCord.

pub mod frame;
pub mod text;

pub use frame::*;
pub use text::*;

/// Length the way JavaScript counts it, in UTF-16 code units.
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

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
