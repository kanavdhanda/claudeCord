//! The command lines: what each command is, and where it is handled. There are two programs, kept completely apart:
//! - `claudecord` (`Cli`), for a machine and its agents: `login` (the only place a hub's address is given, with --hub), `daemon`, `ls`, `attach`,
//!   `stop`, `down`, `doctor`, and what an agent runs in its own shell: `say`, `ask`, `assign`, `done`, `report`, `dump`, `pickup`, `team`, `send`
//! - `claudecord-hub` (`HubCli`), for whoever runs the hub: `serve`, `storage`, `export`, `token`, `web-token`, `uptime`, `probe`, `selftest`
//!
//! Files: `hub`, `storage`, `export`, `uptime` (the hub program's commands), `machine` (daemon, login, attach and friends), `verbs` (the agent's commands).

#[cfg(feature = "hub")]
pub mod export;
#[cfg(feature = "hub")]
pub mod hub;
pub mod machine;
#[cfg(feature = "hub")]
pub mod storage;
#[cfg(feature = "hub")]
pub mod uptime;
pub mod verbs;

use clap::{Parser, Subcommand};

/// Run coding agents on any machine and talk to them like a team.
#[derive(Parser)]
#[command(
    name = "claudecord",
    version,
    about,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// With no command, `claudecord` starts an agent in the current folder and opens its terminal: on a new machine it first opens the
    /// browser to sign in and approve it, and the dashboard page asks which project, name and program. The options below are for that.
    #[command(subcommand)]
    pub command: Option<Cmd>,
    #[command(flatten)]
    pub start: machine::StartArgs,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the daemon on this machine (started for you by `up`).
    Daemon,
    /// Save which hub this machine talks to.
    Login(machine::LoginArgs),
    /// Put the team-chat guide for agents into this folder's AGENTS.md. Adds it to every instruction file already there (AGENTS.md, CLAUDE.md, GEMINI.md, Copilot's),
    /// or makes AGENTS.md; never overwrites your own text, run it again to update. --claude also makes CLAUDE.md.
    Init {
        #[arg(long)]
        claude: bool,
    },
    /// Show or change settings. `settings keep-running on|off`: stay connected to the hub even when no agent is running (off by default).
    Settings {
        name: Option<String>,
        value: Option<String>,
    },
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
    /// Stop one agent, or with no name stop everything here: every agent, and the connection to the hub.
    Stop {
        agent: Option<String>,
        /// Do not ask before stopping everything.
        #[arg(short, long)]
        yes: bool,
    },
    /// Start an agent's program again (every agent here when no name is given). It keeps its name and its place in the team.
    Restart { agent: Option<String> },
    /// Stop the daemon and every agent on this machine.
    Down,
    /// Check whether this machine can reach the hub, and where it stops.
    Doctor,
    /// Post a message to the team (plain say is information; @name someone to need a reply).
    Say {
        /// The message. `-` (or nothing, when something is piped in) reads it from standard input, which keeps real line breaks for multi-line text.
        text: Option<String>,
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
    /// List where you can post: the main channel and your open tasks, each with the id to use in `say --thread`. Answered at once.
    Threads,
    /// Print the short rules for using the team chat (what to run, and when).
    Guide,
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
        use std::io::IsTerminal;
        // Without a terminal there is nobody to choose on the dashboard: only say what is running here.
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return machine::home().await;
        }
        return machine::start(cli.start).await;
    };
    match command {
        Cmd::Daemon => machine::run_daemon().await,
        Cmd::Login(a) => machine::login(a).await,
        Cmd::Logs {
            agent,
            lines,
            terminal,
        } => machine::logs(&agent, lines, terminal).await,
        Cmd::Handoff { agent, out } => machine::handoff(&agent, out).await,
        Cmd::Init { claude } => machine::init(claude).await,
        Cmd::Settings { name, value } => machine::settings(name, value).await,
        Cmd::Ls => machine::ls().await,
        Cmd::Attach { agent } => machine::attach_or_pick(agent).await,
        Cmd::Stop { agent, yes } => machine::stop(agent, yes).await,
        Cmd::Restart { agent } => machine::restart(agent).await,
        Cmd::Down => machine::down().await,
        Cmd::Doctor => machine::doctor().await,
        Cmd::Say { text, thread } => verbs::say(text, thread).await,
        Cmd::Ask { question } => verbs::ask(question).await,
        Cmd::Assign { to, task } => verbs::assign(to, task).await,
        Cmd::Answer { ask, text } => verbs::answer(ask, text).await,
        Cmd::Done { task, summary } => verbs::done(task, summary).await,
        Cmd::Report { title, summary } => verbs::report(title, summary).await,
        Cmd::Dump { text } => verbs::dump(text).await,
        Cmd::Pickup => verbs::pickup().await,
        Cmd::Team => verbs::team().await,
        Cmd::Threads => verbs::threads().await,
        Cmd::Guide => {
            println!("{}", crate::device::daemon::RULES);
            Ok(())
        }
        Cmd::Send { path, to, caption } => verbs::send_file(path, to, caption).await,
    }
}

#[cfg(feature = "hub")]
mod hub_cli;
#[cfg(feature = "hub")]
pub use hub_cli::{HubCli, HubCmd, run_hub_cli};
