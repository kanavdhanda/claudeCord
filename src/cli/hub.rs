//! The server commands: `claudecord hub` runs the central hub, `claudecord token` makes a token for a machine.

use crate::hub::HubCore;
use crate::server::{self, Config};
use crate::store::Store;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct HubArgs {
    /// Where the hub keeps its files.
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
    /// Address to listen on. Put a TLS proxy (Caddy, nginx, Cloudflare) in front for anything but this machine.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub bind: String,
    /// Chat account ids of the owners (repeat for several).
    #[arg(long = "owner")]
    pub owners: Vec<String>,
    /// Allow listening on a public address without TLS. Tokens and messages would travel unencrypted.
    #[arg(long)]
    pub allow_plain: bool,
}

#[derive(Args)]
pub struct TokenArgs {
    /// The machine's name.
    pub node: String,
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
}

/// Makes a token for the dashboard and prints it once. Open the hub's address in a browser and paste it (or add
/// `#token=...` to the address). Only its hash is kept, and it opens the dashboard and nothing else.
pub fn make_web_token(a: TokenArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let mut store = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let token = store
        .create_token(
            &format!("{}{}", crate::server::web::WEB_PREFIX, a.node),
            crate::now_ms(),
        )
        .map_err(|e| e.to_string())?;
    println!("{token}");
    eprintln!(
        "Save this now. It cannot be shown again. Open the hub in a browser and paste it, or add #token=<it> to the address."
    );
    Ok(())
}

/// Runs the hub until interrupted, then saves and tells every machine it is going away.
pub async fn run_hub(a: HubArgs) -> Result<(), String> {
    let bind: std::net::SocketAddr = a
        .bind
        .parse()
        .map_err(|_| format!("{} is not an address like 127.0.0.1:8787", a.bind))?;
    if !bind.ip().is_loopback() && !a.allow_plain {
        return Err("refusing to listen on a public address without TLS: put a TLS proxy in front and bind to 127.0.0.1, or pass --allow-plain if you really mean it".into());
    }
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
        .map_err(|e| e.to_string())?;
    let mut core = HubCore::default();
    for o in &a.owners {
        core.add_owner(o);
    }
    let hub = server::start(
        Config {
            bind,
            ..Config::default()
        },
        core,
        store,
    )
    .await
    .map_err(|e| e.to_string())?;
    println!(
        "hub listening on {}  (data in {})",
        hub.addr,
        a.data.display()
    );
    tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
    println!("shutting down");
    hub.shutdown().await;
    Ok(())
}

/// Makes a token for a machine and prints it once. Only its hash is kept.
pub fn make_token(a: TokenArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let mut store = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let token = store
        .create_token(&a.node, crate::now_ms())
        .map_err(|e| e.to_string())?;
    println!("{token}");
    eprintln!(
        "Save this now. It cannot be shown again. On the machine: claudecord login --hub wss://YOUR-HUB --token <it> --name {}",
        a.node
    );
    Ok(())
}
