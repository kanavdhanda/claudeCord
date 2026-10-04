//! The hub core: the pure state machine that decides who may do what and where every message goes. It does no I/O
//! and reads no clock, so it can be tested completely without a network, a database or Discord.
//!
//! Files, in reading order:
//! - `model`        the vocabulary: roles, agents, asks, tasks, permissions, grants
//! - `effects`      what the core asks the outside world to do
//! - `core`         the struct that holds all state, device connect/disconnect, registration, the frame entry point
//! - `access`       who is allowed to do what
//! - `routing`      queues, coalescing, human messages, `/btw`, agent-to-agent forwarding
//! - `asks`         questions to people and their single winning answer
//! - `permissions`  permission requests, decisions, standing grants
//! - `tasks`        the lead assigning work and peers finishing it
//! - `files`        files between agents, chat and people
//! - `controls`     stop, killall, pause, spawn, choose the lead
//! - `handoff`      saving a session's state when it runs out and picking it up in a fresh one
//! - `expiry`       reminders, expiries, grants running out
//! - `snapshot`     saving and restoring the durable state
//! - `briefs`       the few tokens each agent is told about its role

pub mod access;
pub mod asks;
pub mod briefs;
pub mod controls;
pub mod core;
pub mod effects;
pub mod expiry;
pub mod files;
pub mod handoff;
pub mod model;
pub mod permissions;
pub mod routing;
pub mod snapshot;
pub mod tasks;

pub use self::core::{
    DEFAULT_ACCEPT_TIMEOUT_MS, DEFAULT_GRANT_TTL_MS, DEFAULT_STREAK_LIMIT, HubCore,
    MAX_AGENTS_PER_PROJECT,
};
pub use asks::AskOutcome;
pub use effects::{Chat, Effect, Persist};
pub use model::*;
