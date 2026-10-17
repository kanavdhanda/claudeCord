//! claudeCord Hub: manage coding agents from Discord and web dashboard.

pub mod cli;
pub mod control;
pub mod discord;
pub mod export;
pub mod health;
pub mod hub;
pub mod log;
pub mod metrics;
pub mod notify;
pub mod security;
pub mod server;
pub mod store;
pub mod sync;
pub mod task;
pub mod uptime;

pub use claudecord_protocol as protocol;

pub use discord::perms;
pub use protocol::{jslen, jsslice, now_ms, text};
pub use security::{codes, env, limits, redact};
