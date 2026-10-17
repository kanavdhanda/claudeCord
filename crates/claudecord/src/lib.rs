//! claudeCord: run coding agents on any machine and manage them from a chat.

pub mod agents;
pub mod cli;
pub mod device;
pub mod log;
pub mod security;
pub mod sync;
pub mod task;

pub use claudecord_protocol as protocol;

pub use agents::{adapters, text};
pub use protocol::{jslen, jsslice, now_ms};
pub use security::{codes, env, limits, redact};
