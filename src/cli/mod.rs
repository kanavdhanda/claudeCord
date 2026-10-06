//! The command line: what each command is, and where it is handled. Commands fall in three groups:
//! - running things: `hub` (the central server), `daemon` (the program on a machine)
//! - working with agents: `login`, `up`, `ls`, `attach`, `stop`, `down`, `doctor`
//! - what an agent runs in its own shell: `say`, `ask`, `assign`, `done`, `report`, `dump`, `pickup`, `team`, `send`
//!
//! Files: `hub` (server commands), `discord` (connect to Discord), `storage` (where old history goes), `machine` (daemon, login, up, attach and friends), `verbs` (the agent's commands).

pub mod discord;
pub mod export;
pub mod hub;
pub mod machine;
pub mod storage;
pub mod uptime;
pub mod verbs;

use clap::{Parser, Subcommand};

/// Run coding agents on any machine and talk to them like a team.
#[derive(Parser)]
#[command(name = "claudecord", version, about)]
pub struct Cli {
    /// With no command: on a new machine, open the browser to sign in and approve it; afterwards, say how to start an agent.
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the central hub (one per team).
    Hub(hub::HubArgs),
    /// Run the hosted service: many accounts sign in with Discord, join machines by code, and each gets its own hub.
    Serve(hub::ServeArgs),
    /// Connect the hub to your Discord server (token, server id, invite link).
    Discord(discord::DiscordArgs),
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
    /// Run the daemon on this machine (started for you by `up`).
    Daemon,
    /// Save which hub this machine talks to.
    Login(machine::LoginArgs),
    /// Put the team-chat guide for agents into this folder's AGENTS.md (and CLAUDE.md, for Claude). Never overwrites your own text; run it again to update.
    Init,
    /// Start an agent in the current folder and open its terminal (starting the daemon if needed, and saying so).
    Start(machine::StartArgs),
    /// Show the tail of an agent's log: what it was sent and did, or what its terminal showed.
    Logs {
        agent: String,
        #[arg(long, default_value_t = 40)]
        lines: usize,
        /// Show what its terminal showed instead.
        #[arg(long)]
        terminal: bool,
    },
    /// Write a note for carrying an agent's work on: what happened lately and what its terminal last showed.
    Handoff {
        agent: String,
        /// Write to this file instead of printing.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// List the agents on this machine.
    Ls,
    /// Open an agent's terminal. With no name, pick from the agents running here.
    Attach { agent: Option<String> },
    /// Stop one agent.
    Stop { agent: String },
    /// Stop the daemon and every agent on this machine.
    Down,
    /// Check whether this machine can reach the hub, and where it stops.
    Doctor,
    /// Check that every feature is alive (starts real servers on this machine; changes nothing).
    Selftest {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Post a message to the team (plain say is information; @name someone to need a reply).
    Say {
        text: String,
        #[arg(long)]
        thread: Option<String>,
    },
    /// Ask a question. Returns at once: end your turn, the answer arrives as your next message.
    Ask { question: String },
    /// Give a task to a peer (the lead only).
    Assign { to: String, task: String },
    /// Answer another agent's question (a person answers in Discord).
    Answer { ask: String, text: String },
    /// Report a task finished.
    Done { task: String, summary: String },
    /// Send a report to the people.
    Report { title: String, summary: String },
    /// Save this session's state so a fresh session can carry on.
    Dump { text: String },
    /// Ask what a fresh session should carry on from.
    Pickup,
    /// Ask who else is in this project, what they do and whether they can be reached. The answer arrives as your next input.
    Team,
    /// Send a file to the chat, or to a peer with --to.
    Send {
        path: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        caption: Option<String>,
    },
}

/// Runs a parsed command.
pub async fn run(cli: Cli) -> Result<(), String> {
    let Some(command) = cli.command else {
        return machine::home().await;
    };
    match command {
        Cmd::Hub(a) => hub::run_hub(a).await,
        Cmd::Serve(a) => hub::run_serve(a).await,
        Cmd::Token(a) => hub::make_token(a),
        Cmd::WebToken(a) => hub::make_web_token(a),
        Cmd::LoadTokens(a) => hub::make_load_tokens(a),
        Cmd::Uptime(a) => uptime::show(a),
        Cmd::Probe(a) => uptime::probe(a).await,
        Cmd::Discord(a) => discord::run(a),
        Cmd::Storage(a) => storage::run(a),
        Cmd::Export(a) => export::run(a),
        Cmd::Daemon => machine::run_daemon().await,
        Cmd::Login(a) => machine::login(a).await,
        Cmd::Start(a) => machine::start(a).await,
        Cmd::Logs {
            agent,
            lines,
            terminal,
        } => machine::logs(&agent, lines, terminal).await,
        Cmd::Handoff { agent, out } => machine::handoff(&agent, out).await,
        Cmd::Init => machine::init().await,
        Cmd::Ls => machine::ls().await,
        Cmd::Attach { agent } => machine::attach_or_pick(agent).await,
        Cmd::Stop { agent } => machine::stop(&agent).await,
        Cmd::Down => machine::down().await,
        Cmd::Doctor => machine::doctor().await,
        Cmd::Selftest { json } => selftest(json).await,
        Cmd::Say { text, thread } => verbs::say(text, thread).await,
        Cmd::Ask { question } => verbs::ask(question).await,
        Cmd::Assign { to, task } => verbs::assign(to, task).await,
        Cmd::Answer { ask, text } => verbs::answer(ask, text).await,
        Cmd::Done { task, summary } => verbs::done(task, summary).await,
        Cmd::Report { title, summary } => verbs::report(title, summary).await,
        Cmd::Dump { text } => verbs::dump(text).await,
        Cmd::Pickup => verbs::pickup().await,
        Cmd::Team => verbs::team().await,
        Cmd::Send { path, to, caption } => verbs::send_file(path, to, caption).await,
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
