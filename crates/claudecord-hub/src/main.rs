//! The `claudecord-hub` program: runs the hosted hub and looks after it (storage, tokens, uptime, export).

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = claudecord_hub::cli::HubCli::parse();
    if let Err(e) = claudecord_hub::cli::run_hub_cli(cli).await {
        eprintln!("claudecord-hub: {e}");
        std::process::exit(1);
    }
}
