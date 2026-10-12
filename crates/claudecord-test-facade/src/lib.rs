//! Test facade combining client, hub, and protocol for integration tests.

pub use claudecord_client::log;
pub use claudecord_client::security;
pub use claudecord_client::sync;
pub use claudecord_client::task;
pub use claudecord_client::{agents, device};
pub use claudecord_hub::{
    control, discord, export, health, hub, metrics, notify, server, store, uptime,
};
pub use claudecord_protocol as protocol;
pub use claudecord_protocol::*;

pub use agents::{adapters, text};
pub use discord::perms;
pub use security::{codes, env, limits, redact};

pub mod cli {
    pub use claudecord_client::cli::*;
    pub use claudecord_hub::cli::*;
}
