//! The `claudecord-hub` program: runs the hosted hub and looks after it (storage, tokens, uptime, export). It shares no commands with
//! `claudecord`, which is the program a machine runs. This file only parses the command line and hands off to `cli`.

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = claudecord::cli::HubCli::parse();
    if let Err(e) = claudecord::cli::run_hub_cli(cli).await {
        eprintln!("claudecord-hub: {e}");
        std::process::exit(1);
    }
}
