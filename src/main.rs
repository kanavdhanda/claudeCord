//! The `claudecord` program. One binary does every job: run the hub, run the daemon on a machine, start and watch agents,
//! and the short commands an agent runs in its shell (`claudecord say ...`). This file only parses the command line and
//! hands off to `cli`.

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = claudecord::cli::Cli::parse();
    if let Err(e) = claudecord::cli::run(cli).await {
        eprintln!("claudecord: {e}");
        std::process::exit(1);
    }
}
