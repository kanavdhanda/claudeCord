//! Every integration test, built as ONE program so the whole tree is compiled and linked once (twenty separate test programs
//! meant twenty links, which is most of the time a CI run spends, above all on Windows). Each file stays a separate module with its
//! own purpose and its own helpers; run one with `cargo test --test it races::` (the module name, then `::`).

pub fn bin(name: &str) -> std::path::PathBuf {
    let key = format!("CARGO_BIN_EXE_{name}");
    if let Ok(p) = std::env::var(&key) {
        return std::path::PathBuf::from(p);
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap_or(manifest);
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root.join("target"));
    let debug = target.join("debug").join(name);
    if debug.exists() {
        return debug;
    }
    let release = target.join("release").join(name);
    if release.exists() {
        return release;
    }
    debug
}

mod bucket;
mod capacity;
mod conformance;
mod device_link;
mod device_pty;
mod device_tmux;
mod discord_bridge;
mod durability;
mod e2e;
mod edge;
mod export;
mod gateway;
mod health;
mod hub;
mod logs;
mod procs;
mod races;
mod resilience;
mod robust;
mod server;
mod store;
mod tiering;
mod token_budget;
mod uptime;
mod web_login;
