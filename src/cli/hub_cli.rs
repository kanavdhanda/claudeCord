//! The command line of `claudecord-hub`, the program the hub's Docker image runs. It exists only in a build with the `hub` feature.

use super::{export, hub, storage, uptime};
use clap::{Parser, Subcommand};

/// The `claudecord-hub` program: the server side, kept apart from `claudecord` (the machine and agent side). It is never needed on a
/// machine that only runs agents, and a machine never needs the other one's commands.
#[derive(Parser)]
#[command(
    name = "claudecord-hub",
    version,
    about = "Run and look after a claudeCord hub"
)]
pub struct HubCli {
    #[command(subcommand)]
    pub command: HubCmd,
}

#[derive(Subcommand)]
pub enum HubCmd {
    /// Run a bare hub with no Discord, for tests and development. Real use is `serve`.
    #[command(hide = true)]
    Hub(hub::HubArgs),
    /// Run the hosted service: many accounts sign in with Discord, join machines by code, and each gets its own hub.
    Serve(hub::ServeArgs),
    /// Choose, test and change where old history files are kept (Oracle Cloud, Cloudflare R2, ...).
    Storage(storage::StorageArgs),
    /// Write the conversation history as Obsidian notes you can browse and graph.
    Export(export::ExportArgs),
    /// Make a token for a machine (run on the hub's host).
    Token(hub::TokenArgs),
    /// Show how much of the time the hub and Discord were working, and the error budget left.
    Uptime(uptime::UptimeArgs),
    /// Check a hub from the outside (run on another machine) and keep a record of what was seen.
    Probe(uptime::ProbeArgs),
    /// Make many machine tokens at once into a private file, for the k6 load test.
    LoadTokens(hub::LoadTokensArgs),
    /// Make a token that opens the dashboard in a browser (run on the hub's host).
    WebToken(hub::TokenArgs),
    /// Check that every feature is alive (starts real servers on this machine; changes nothing).
    Selftest {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Runs a parsed `claudecord-hub` command.
pub async fn run_hub_cli(cli: HubCli) -> Result<(), String> {
    match cli.command {
        HubCmd::Hub(a) => hub::run_hub(a).await,
        HubCmd::Serve(a) => hub::run_serve(a).await,
        HubCmd::Token(a) => hub::make_token(a),
        HubCmd::WebToken(a) => hub::make_web_token(a),
        HubCmd::LoadTokens(a) => hub::make_load_tokens(a),
        HubCmd::Uptime(a) => uptime::show(a),
        HubCmd::Probe(a) => uptime::probe(a).await,
        HubCmd::Storage(a) => storage::run(a),
        HubCmd::Export(a) => export::run(a),
        HubCmd::Selftest { json } => selftest(json).await,
    }
}

/// Runs the health check and prints a line per feature. Fails if any feature is not working.
async fn selftest(json: bool) -> Result<(), String> {
    let outcomes = crate::health::run_all().await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcomes).expect("plain data")
        );
    } else {
        for o in &outcomes {
            println!(
                "{:5} {:44} {}",
                if o.ok { "PASS" } else { "FAIL" },
                o.name,
                o.detail
            );
        }
    }
    let failed = outcomes.iter().filter(|o| !o.ok).count();
    if failed > 0 {
        return Err(format!(
            "{failed} of {} features are not working",
            outcomes.len()
        ));
    }
    if !json {
        println!("all {} features alive", outcomes.len());
    }
    Ok(())
}
