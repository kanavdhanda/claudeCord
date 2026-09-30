//! Everything about Discord: the permissions the bot needs, a client for its REST API, the live gateway connection, and the
//! bridge that connects all of that to the hub. Nothing outside this folder knows Discord exists: the hub core only
//! produces "show this in the chat" effects and accepts messages from named people.
//!
//! Files: `perms` (what the bot may do), `api` (REST calls), `gateway` (live events), `bridge` (the two-way connection),
//! `commands` (the slash commands and what each one does), `fake` (a stand-in Discord for tests and the self-check).

pub mod api;
pub mod bridge;
pub mod commands;
#[doc(hidden)]
pub mod fake;
pub mod gateway;
pub mod perms;
