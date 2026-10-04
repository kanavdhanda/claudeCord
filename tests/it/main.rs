//! Every integration test, built as ONE program so the whole tree is compiled and linked once (twenty separate test programs
//! meant twenty links, which is most of the time a CI run spends, above all on Windows). Each file stays a separate module with its
//! own purpose and its own helpers; run one with `cargo test --test it races::` (the module name, then `::`).

mod bucket;
mod conformance;
mod device_link;
mod device_pty;
mod device_tmux;
mod discord_bridge;
mod durability;
mod e2e;
mod edge;
mod export;
mod health;
mod hub;
mod logs;
mod races;
mod resilience;
mod robust;
mod server;
mod store;
mod tiering;
mod token_budget;
mod uptime;
mod web_login;
