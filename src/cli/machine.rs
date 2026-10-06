//! Commands for a machine that runs agents: save the hub address, run the daemon, start an agent in the current folder,
//! open an agent's terminal, list, stop, and check connectivity. The daemon is started for you the first time it is needed.

use crate::device::config::{Config, home_dir};
use crate::device::daemon::{self, Options};
use crate::device::doctor;
use crate::device::enroll;
use crate::device::ipc::{self, Req, Resp, UpOpts};
use crate::device::link::LinkOpts;
use clap::Args;
use interprocess::local_socket::traits::tokio::Stream as _;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Args)]
pub struct LoginArgs {
    /// Where the hub is, for example https://claudecord.example.com (default: the address this program was built for, or $CLAUDECORD_HUB).
    #[arg(long)]
    pub hub: Option<String>,
    /// A token the hub gave this machine. Without it, a browser opens for you to sign in and approve this machine.
    #[arg(long)]
    pub token: Option<String>,
    /// A name for this machine (default: its hostname).
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct StartArgs {
    /// Name of the project (default: this folder's name).
    #[arg(long)]
    pub project: Option<String>,
    /// Name for this agent (default: a friendly random one).
    #[arg(long)]
    pub name: Option<String>,
    /// Which agent program to run: claude, agy or codex.
    #[arg(long, default_value = "claude")]
    pub adapter: String,
    #[arg(long)]
    pub model: Option<String>,
    /// A short description of this agent's job, shown to the others.
    #[arg(long)]
    pub role: Option<String>,
    /// autonomous, plan or ask.
    #[arg(long, default_value = "ask")]
    pub policy: String,
    /// Start it and leave it running without opening its terminal.
    #[arg(long)]
    pub detach: bool,
    /// If another agent here already works in this folder, give this one its own git worktree. Otherwise that start is refused.
    #[arg(long)]
    pub worktree: bool,
    /// Ask the hub for a saved handoff to carry on from. Otherwise the agent starts clean.
    #[arg(long)]
    pub pickup: bool,
    /// Start the agent again up to this many times (in ten minutes) if its program ends. Otherwise never.
    #[arg(long, default_value_t = 0)]
    pub restart: u32,
    /// Choose the project on the dashboard again, even if this folder already belongs to one.
    #[arg(long)]
    pub pick: bool,
    /// Do not write the team-chat guide into this folder's AGENTS.md.
    #[arg(long)]
    pub no_guide: bool,
    /// Run this command instead of the agent program (anything that runs in a terminal). Put it after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// Joins a hub: with a token, saves it; without one, opens the browser to sign in with Discord and approve this machine.
pub async fn login(a: LoginArgs) -> Result<(), String> {
    let hub = a
        .hub
        .or_else(|| std::env::var("CLAUDECORD_HUB").ok())
        .unwrap_or_else(|| enroll::default_hub().to_string());
    enroll::check_hub(&hub)?;
    let name = a.name.unwrap_or_else(enroll::default_node_name);
    let cfg = match a.token {
        Some(token) => Config {
            hub_url: enroll::ws_base(&hub),
            token,
            node_name: name,
        },
        None => browser_login(&hub, &name).await?,
    };
    cfg.save(&home_dir()).map_err(|e| e.to_string())?;
    println!(
        "saved. This machine is connected. Next: {} start, in the folder of a project.",
        me()
    );
    Ok(())
}

/// What bare `claudecord` (and `npx claudecord`) does. A machine that has never joined opens the browser to sign in and approve it. One that has
/// shows the agents here and the ways to start one, to choose from (nothing starts by itself). Without a terminal to ask on it only says what is running.
pub async fn home() -> Result<(), String> {
    use std::io::IsTerminal;
    if let Some(cfg) = Config::load(&home_dir()) {
        // The daemon is never started just to look: if it is not running, nothing is.
        let rows = list_agents().await;
        if let Some(n) = update_notice() {
            println!("{n}\n");
        }
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            println!("Connected to {} as {}.", cfg.hub_url, cfg.node_name);
            println!("Running here: {}", rows.len());
            for a in &rows {
                println!(
                    "  {:28} {:12} {}",
                    a["agent"].as_str().unwrap_or(""),
                    a["status"].as_str().unwrap_or(""),
                    a["cwd"].as_str().unwrap_or("")
                );
            }
            println!("Start an agent in a project folder:  {} start", me());
            return Ok(());
        }
        return pick_and_attach(&rows, true).await;
    }
    login(LoginArgs {
        hub: None,
        token: None,
        name: None,
    })
    .await?;
    println!(
        "Connected. Start an agent with `{} start` in a project folder.",
        me()
    );
    Ok(())
}

/// The first-run path: shows a code and link, opens the browser, and waits for the person to approve this machine.
async fn browser_login(hub: &str, name: &str) -> Result<Config, String> {
    enroll::enroll(hub, name, |code, link| {
        eprintln!("To connect this machine, sign in with Discord and approve it.");
        eprintln!("  Code: {code}");
        eprintln!("  Link: {link}");
        eprintln!("Waiting for you to approve it in the browser...");
        enroll::open_browser(link);
    })
    .await
}

/// Runs the daemon in the foreground.
pub async fn run_daemon() -> Result<(), String> {
    let dir = home_dir();
    let cfg =
        Config::load(&dir).ok_or_else(|| format!("not logged in: run {} login first", me()))?;
    // The daemon logs to the terminal, which `claudecord start` points at daemon.log when it starts the daemon for you.
    crate::log::init(None);
    // Gone by itself 20 seconds after the last agent ends, unless the person turned on keep-running.
    let opts = Options {
        idle_exit: Some(Duration::from_secs(20)),
        ..Options::default()
    };
    // The daemon is started from a terminal. Closing that terminal sends it a hangup, whose default is to end it (and with it every agent's
    // supervision): a daemon is meant to outlive the window it was started in.
    #[cfg(unix)]
    let _hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    daemon::run(cfg, dir, opts).await.map_err(|e| e.to_string())
}

/// A machine that has never joined: this is the first run, so sign in through the browser now rather than failing.
async fn ensure_login() -> Result<(), String> {
    if Config::load(&home_dir()).is_none() {
        login(LoginArgs {
            hub: None,
            token: None,
            name: None,
        })
        .await?;
    }
    Ok(())
}

/// The one line that says a newer claudeCord exists, if the hub told this machine so (the daemon keeps what it was told in `update.json`), with
/// the command that updates it the way it was installed. None when this is already the newest, or nothing was said.
fn update_notice() -> Option<String> {
    let path = home_dir().join("update.json");
    let latest = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).ok()?)
        .ok()?["latest"]
        .as_str()?
        .to_string();
    let mine = env!("CARGO_PKG_VERSION");
    if !crate::protocol::version_older(mine, &latest) {
        // Updated since: nothing left to say.
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(format!(
        "claudecord {latest} is available (you have {mine}). Update with:  {}",
        update_command(&std::env::current_exe().ok()?.to_string_lossy())
    ))
}

/// How to update, judged by where this program lives: under uv's tools, an npm package, or (otherwise) a pip install.
fn update_command(exe: &str) -> &'static str {
    let e = exe.replace('\\', "/");
    if e.contains("/uv/tools/") || e.contains("/uv/") && e.contains("/tools/") {
        "uv tool upgrade claudecord"
    } else if e.contains("node_modules") {
        "npm i -g claudecord@latest"
    } else if e.contains("/.cargo/bin/") {
        "cargo install --git https://github.com/kanavdhanda/claudeCord --force"
    } else {
        "pip install -U claudecord   (or: pipx upgrade claudecord)"
    }
}

/// Whether `program` is found in a folder of the PATH.
fn program_on_path(program: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| {
        exts.iter()
            .any(|e| dir.join(format!("{program}{e}")).is_file())
    })
}

/// What the machine must have whichever agent is started (tmux), checked before the person is sent to the dashboard.
fn require_machine() -> Result<(), String> {
    match doctor::tmux_missing() {
        Some(m) => Err(format!("cannot start an agent here yet: {m}")),
        None => Ok(()),
    }
}

/// The program of the chosen agent type, checked once the page has chosen it (unless a command of the person's own is given).
fn require_program(adapter: &str, own_command: bool) -> Result<(), String> {
    let program = doctor::program_of(adapter);
    if own_command || program_on_path(program) {
        return Ok(());
    }
    Err(format!(
        "cannot start an agent here yet: {}",
        doctor::program_missing(program)
    ))
}

/// Makes sure a daemon is listening, starting one in the background if not.
async fn ensure_daemon() -> Result<(), String> {
    let dir = home_dir();
    if ipc::call(&dir, &Req::Ping).await.is_ok() {
        return Ok(());
    }
    ensure_login().await?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Appended to, so the history of earlier runs is still there when something went wrong.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| format!("could not start the daemon: {e}"))?;
    println!(
        "started the claudecord daemon (its log: {})",
        dir.join("daemon.log").display()
    );
    for _ in 0..50 {
        if ipc::call(&dir, &Req::Ping).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "the daemon did not start; see {}",
        dir.join("daemon.log").display()
    ))
}

/// Turns an answer into a result.
fn expect_ok(r: Resp) -> Result<Resp, String> {
    if r.ok { Ok(r) } else { Err(r.msg) }
}

/// `claudecord start`: starts the daemon if it is not running, asks which project this folder belongs to (the first time), starts an agent in the
/// current folder, writes the team guide into AGENTS.md, opens the dashboard if the project has no Discord channel yet, and opens the agent's terminal
/// unless asked not to. Nothing else: no handoff, no restarts, no shared folders.
pub async fn start(a: StartArgs) -> Result<(), String> {
    // Before anything is asked of the person (signing in, choosing a project on the dashboard), not after.
    if !["claude", "agy", "codex"].contains(&a.adapter.as_str()) {
        return Err(format!(
            "unknown agent type {}: claude, agy or codex",
            a.adapter
        ));
    }
    if !["autonomous", "plan", "ask"].contains(&a.policy.as_str()) {
        return Err(format!(
            "unknown policy {}: autonomous, plan or ask",
            a.policy
        ));
    }
    // Everything the machine must have comes first, so nobody fills in the dashboard only to learn that tmux is missing.
    require_machine()?;
    ensure_login().await?;
    if let Some(n) = update_notice() {
        println!("{n}\n");
    }
    let dir = home_dir();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // The project is chosen on the dashboard, never here, and before the daemon starts (a person may take longer than the daemon's idle wait).
    let mut picked = false;
    let mut chosen = Picked::default();
    let project = match a.project.clone() {
        Some(p) => p,
        None => {
            let known = crate::device::config::project_of_folder(&dir, &cwd);
            match known.clone().filter(|_| !a.pick) {
                Some(p) => p,
                None => {
                    picked = true;
                    // The folder's own project (if it has one) is what the page offers first; the person can change it there.
                    chosen = pick_on_dashboard(&cwd, known.as_deref()).await?;
                    chosen.project.clone()
                }
            }
        }
    };
    // Everything from here can fail on this machine (the program is not installed, the daemon does not come up, the name is taken). The page
    // that asked for the agent is told which, so the person sees it there and not only in this terminal.
    let started = async {
        // No name given: the daemon makes a friendly one (shown below and in the agent's window).
        wait_until_connected(&project, picked).await?;
        // What was typed on the command line wins over what the page sent.
        let name = a.name.clone().or(chosen.agent.clone());
        let adapter = chosen
            .adapter
            .clone()
            .filter(|_| a.adapter == "claude")
            .unwrap_or(a.adapter.clone());
        require_program(&adapter, !a.command.is_empty())?;
        ensure_daemon().await?;
        let (cols, rows) = terminal_size();
        let r = ipc::call(
            &dir,
            &Req::Up {
                project: project.clone(),
                name,
                adapter,
                model: a.model.clone(),
                role: a.role.clone().or(chosen.role.clone()),
                cwd: cwd.to_string_lossy().into(),
                policy: a.policy.clone(),
                rows,
                cols,
                opts: UpOpts {
                    command: (!a.command.is_empty()).then_some(a.command.clone()),
                    worktree: a.worktree,
                    pickup: a.pickup,
                    restart: a.restart,
                },
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        expect_ok(r)
    }
    .await;
    if !chosen.code.is_empty() {
        match &started {
            Ok(r) => report_pick(&chosen.code, true, &r.msg).await,
            Err(e) => report_pick(&chosen.code, false, e).await,
        }
    }
    let reply = started?;
    let agent = reply.msg.clone();
    println!("started {agent}");
    if let Some(tree) = reply.data.as_ref().and_then(|d| d["worktree"].as_str()) {
        println!(
            "another agent already works in this folder, so {agent} has its own git worktree: {tree}"
        );
    }
    if !a.no_guide {
        for line in install_guide(&cwd, false)? {
            if line.starts_with("wrote") {
                println!("{line}");
            }
        }
    }
    if a.detach {
        println!("attach later with: {} attach {agent}", me());
        return Ok(());
    }
    println!(
        "In the agent: Ctrl-] leaves it running. /exit (or {} stop {agent}) ends it. Come back with: {} attach {agent}",
        me(),
        me()
    );
    attach(&agent).await
}

/// What the dashboard's start page sent back: the project, and the first agent's name and program if the person set them.
#[derive(Default)]
struct Picked {
    project: String,
    agent: Option<String>,
    adapter: Option<String>,
    role: Option<String>,
    /// The page's code, so what happens next can be reported back to it.
    code: String,
}

/// Has the person choose, on the dashboard, which project this folder belongs to: the hub gives a short code, the dashboard's pick page shows
/// it, and this waits for the answer. If the hub is an older one with no such page, the folder's name is used.
async fn pick_on_dashboard(cwd: &std::path::Path, current: Option<&str>) -> Result<Picked, String> {
    let folder = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let cfg = Config::load(&home_dir()).ok_or("not logged in")?;
    let base = enroll::http_base(&cfg.hub_url);
    let http = reqwest::Client::new();
    let asked = http
        .post(format!("{base}/api/device/pick"))
        .bearer_auth(&cfg.token)
        .json(&serde_json::json!({ "folder": folder, "project": current }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let code = match asked {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["code"].as_str().map(String::from)),
        _ => None,
    };
    let Some(code) = code else {
        println!(
            "This hub cannot ask you on the dashboard, so the project is named after the folder: {folder}"
        );
        return Ok(Picked {
            project: folder,
            ..Default::default()
        });
    };
    let url = format!("{base}/pick?code={code}");
    println!("Choose which project \"{folder}\" belongs to, on the dashboard:\n  {url}");
    println!("Waiting for your choice... (Ctrl-C to cancel; nothing has been started)");
    enroll::open_browser(&url);
    for _ in 0..3600 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(r) = http
            .get(format!("{base}/api/device/pick/{code}"))
            .bearer_auth(&cfg.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        else {
            continue;
        };
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(format!("that choice expired: run {} start again", me()));
        }
        if let Ok(v) = r.json::<serde_json::Value>().await
            && let Some(p) = v["chosen"].as_str()
        {
            println!("project: {p}");
            return Ok(Picked {
                project: p.to_string(),
                agent: v["agent"].as_str().map(String::from),
                adapter: v["adapter"].as_str().map(String::from),
                role: v["role"].as_str().map(String::from),
                code: code.clone(),
            });
        }
    }
    Err(format!(
        "no choice was made in an hour: run {} start again",
        me()
    ))
}

/// Tells the dashboard page what became of the agent it asked for: started, or why not. A hub too old to take it is not a problem.
async fn report_pick(code: &str, ok: bool, what: &str) {
    let Some(cfg) = Config::load(&home_dir()) else {
        return;
    };
    let _ = reqwest::Client::new()
        .post(format!(
            "{}/api/device/pick/{code}/result",
            enroll::http_base(&cfg.hub_url)
        ))
        .bearer_auth(&cfg.token)
        .json(&serde_json::json!({ "ok": ok, "message": what }))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
}

/// Waits until the project is connected to a Discord channel where its bot can really work, before anything is started: no agent is made for a
/// project whose messages would go nowhere. If the bot was later removed from the server or lost a permission, it says what, and carries on by
/// itself once that is fixed. A hub too old to be asked, or one that cannot be reached, never blocks a start. Without a person at the keyboard
/// it only says what is missing.
async fn wait_until_connected(project: &str, just_chose: bool) -> Result<(), String> {
    use std::io::IsTerminal;
    let Some(cfg) = Config::load(&home_dir()) else {
        return Ok(());
    };
    let base = enroll::http_base(&cfg.hub_url);
    let http = reqwest::Client::new();
    let mut said: Option<String> = None;
    for _ in 0..450 {
        let asked = http
            .get(format!("{base}/api/device/project/{project}"))
            .bearer_auth(&cfg.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let answer = match asked {
            Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok(),
            _ => None,
        };
        let Some(v) = answer.filter(|v| v.get("placed").is_some()) else {
            return Ok(());
        };
        let problem = v["problem"].as_str().map(String::from);
        if v["placed"].as_bool() == Some(true) && problem.is_none() {
            if said.is_some() {
                println!("connected.");
            }
            return Ok(());
        }
        let now = problem.clone().unwrap_or_else(|| "unplaced".into());
        if said.as_deref() != Some(now.as_str()) {
            let (url, line) = match &problem {
                Some(why) => (
                    format!("{base}/bots"),
                    format!(
                        "The project \"{project}\" is set up in Discord, but it cannot work right now: {why}"
                    ),
                ),
                None => (
                    format!("{base}/setup?project={project}"),
                    format!("The project \"{project}\" is not connected to a Discord channel yet."),
                ),
            };
            println!("{line}\n  Set it up here: {url}");
            if !std::io::stdin().is_terminal() {
                return Ok(());
            }
            println!(
                "Waiting for that to be fixed... (Ctrl-C to cancel; nothing has been started)"
            );
            if problem.is_none() && !just_chose {
                enroll::open_browser(&url);
            }
            said = Some(now);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!(
        "the project \"{project}\" was still not connected to Discord after 15 minutes: run {} start again once it is",
        me()
    ))
}

/// Lists the agents on this machine.
pub async fn ls() -> Result<(), String> {
    let r = expect_ok(
        ipc::call(&home_dir(), &Req::List)
            .await
            .map_err(|_| "no daemon is running here".to_string())?,
    )?;
    for a in r
        .data
        .and_then(|d| d.as_array().cloned())
        .unwrap_or_default()
    {
        println!(
            "{:30} {}",
            a["agent"].as_str().unwrap_or(""),
            a["status"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Stops one agent, or with no name everything here: every agent and the daemon. Everything asks first (unless `yes`, or nobody is there to ask).
pub async fn stop(agent: Option<String>, yes: bool) -> Result<(), String> {
    let Some(agent) = agent else {
        let rows = list_agents().await;
        if ipc::call(&home_dir(), &Req::Ping).await.is_err() {
            println!("nothing was running here");
            return Ok(());
        }
        if !yes && !rows.is_empty() && !confirm(&rows) {
            println!("nothing stopped");
            return Ok(());
        }
        let _ = ipc::call(&home_dir(), &Req::Shutdown).await;
        // The daemon answers first and goes a moment later: say "stopped" only once it has.
        for _ in 0..30 {
            if ipc::call(&home_dir(), &Req::Ping).await.is_err() {
                println!(
                    "stopped {} agent(s) and disconnected this machine",
                    rows.len()
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        return Err("asked the daemon to stop, but it is still answering; see daemon.log in the claudecord folder".into());
    };
    expect_ok(
        ipc::call(
            &home_dir(),
            &Req::Stop {
                agent: agent.clone(),
            },
        )
        .await
        .map_err(|_| "no daemon is running here".to_string())?,
    )?;
    println!("stopped {agent}");
    Ok(())
}

/// Starts an agent's program again (every agent here with no name). It keeps its name, folder and place in the team.
pub async fn restart(agent: Option<String>) -> Result<(), String> {
    let r = ipc::call(&home_dir(), &Req::Restart { agent })
        .await
        .map_err(|_| "nothing is running here".to_string())?;
    if !r.ok && r.msg == "bad request" {
        // The daemon running here was started by an older build of this program and does not know the request.
        return Err(format!(
            "the daemon running here is from an older version and cannot restart agents. Run `{} stop` (that ends its agents) and start them again",
            me()
        ));
    }
    println!("{}", expect_ok(r)?.msg);
    Ok(())
}

/// Stops the daemon, which stops every agent, without asking.
pub async fn down() -> Result<(), String> {
    stop(None, true).await
}

/// Checks whether this machine can reach the hub and prints each step.
pub async fn doctor() -> Result<(), String> {
    let cfg = Config::load(&home_dir())
        .ok_or_else(|| format!("not logged in: run {} login first", me()))?;
    let checks = doctor::run(&cfg, &LinkOpts::default()).await;
    let mut bad = false;
    for c in &checks {
        println!(
            "{:10} {}  {}",
            c.name,
            if c.ok { "ok  " } else { "FAIL" },
            c.detail
        );
        bad |= !c.ok;
    }
    if bad {
        Err("some checks failed".into())
    } else {
        Ok(())
    }
}

/// The size of the person's terminal as (columns, rows), or a default when it cannot be read (not a terminal).
fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((100, 30))
}

/// Puts the person's terminal in raw mode and restores it when dropped, even if the program ends early.
struct RawMode;

impl RawMode {
    fn enter() -> Self {
        let _ = crossterm::terminal::enable_raw_mode();
        RawMode
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// The frame that tells the daemon the window size: type 1, then rows and columns.
fn resize_frame() -> Vec<u8> {
    let (cols, rows) = terminal_size();
    let mut f = vec![1u8, 0, 4];
    f.extend_from_slice(&rows.to_be_bytes());
    f.extend_from_slice(&cols.to_be_bytes());
    f
}

/// The bytes a terminal program expects for a key press. Used on Windows, where the console reports key events rather than bytes.
pub fn key_bytes(code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match code {
        KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() => {
            vec![(c.to_ascii_lowercase() as u8) - b'a' + 1]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => vec![],
    }
}

/// Opens an agent's terminal in this window. With tmux this runs `tmux attach`, so everything about tmux works as usual (detach
/// with its own keys). Otherwise keys go to the agent, its output comes back, and the window size follows; Ctrl-] leaves.
pub async fn attach(agent: &str) -> Result<(), String> {
    let mut s = ipc::connect(&home_dir())
        .await
        .map_err(|_| "no daemon is running here".to_string())?;
    let mut line = serde_json::to_string(&Req::Attach {
        agent: agent.into(),
    })
    .expect("plain data");
    line.push('\n');
    s.write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let (rd, mut wr) = s.split();
    let mut rd = BufReader::new(rd);
    let mut reply = String::new();
    rd.read_line(&mut reply).await.map_err(|e| e.to_string())?;
    let resp: Resp = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    let resp = expect_ok(resp)?;
    if let Some(cmd) = resp.data.as_ref().and_then(|d| d["exec"].as_array()) {
        let cmd: Vec<String> = cmd
            .iter()
            .filter_map(|c| c.as_str().map(String::from))
            .collect();
        let status = Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .map_err(|e| format!("could not run {}: {e}", cmd[0]))?;
        if !status.success() {
            return Err(format!("{} ended with {status}", cmd[0]));
        }
        if still_running(agent).await {
            println!(
                "left {agent}; it is still running. {} attach {agent} to return, {} stop {agent} to end it.",
                me(),
                me()
            );
        } else {
            println!("{agent} has ended.");
        }
        return Ok(());
    }
    let _raw = RawMode::enter();
    let _ = wr.write_all(&resize_frame()).await;
    let mut stdout = tokio::io::stdout();
    let (keys_tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    spawn_keys(keys_tx);
    let mut outbuf = [0u8; 8192];
    let mut size = terminal_size();
    let mut watch = tokio::time::interval(Duration::from_millis(400));
    loop {
        tokio::select! {
            k = keys.recv() => {
                let Some(k) = k else { break };
                if k.contains(&0x1d) { break; }
                let mut f = vec![0u8];
                f.extend_from_slice(&(k.len() as u16).to_be_bytes());
                f.extend_from_slice(&k);
                if wr.write_all(&f).await.is_err() { break; }
            }
            n = rd.read(&mut outbuf) => {
                let n = n.map_err(|e| e.to_string())?;
                if n == 0 { break; }
                let _ = stdout.write_all(&outbuf[..n]).await;
                let _ = stdout.flush().await;
            }
            _ = watch.tick() => {
                // The window was resized: tell the agent's terminal.
                let now = terminal_size();
                if now != size { size = now; let _ = wr.write_all(&resize_frame()).await; }
            }
        }
    }
    println!(
        "\r\nleft {agent}; it is still running ({} attach {agent} to return)",
        me()
    );
    Ok(())
}

/// Reads the person's keys on a separate thread and sends the bytes along. On unix that is the raw bytes from standard input.
/// On Windows it is the console's key events turned into the bytes a terminal program expects.
fn spawn_keys(tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    #[cfg(unix)]
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 1024];
        while let Ok(n) = std::io::stdin().read(&mut buf) {
            if n == 0 || tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    #[cfg(not(unix))]
    std::thread::spawn(move || {
        use crossterm::event::{Event, KeyEventKind, read};
        while let Ok(ev) = read() {
            if let Event::Key(k) = ev
                && k.kind != KeyEventKind::Release
            {
                let b = key_bytes(k.code, k.modifiers);
                if !b.is_empty() && tx.blocking_send(b).is_err() {
                    break;
                }
            }
        }
    });
}

/// `claudecord logs`: prints the tail of an agent's log. By default what happened (what it was sent, what it said, decisions);
/// with `--terminal` what its terminal showed.
pub async fn logs(agent: &str, lines: usize, terminal: bool) -> Result<(), String> {
    let r = expect_ok(
        ipc::call(
            &home_dir(),
            &Req::Logs {
                agent: agent.into(),
                lines,
                terminal,
            },
        )
        .await
        .map_err(|_| "no daemon is running here".to_string())?,
    )?;
    for l in r
        .data
        .and_then(|d| d["lines"].as_array().cloned())
        .unwrap_or_default()
    {
        println!("{}", l.as_str().unwrap_or(""));
    }
    Ok(())
}

/// `claudecord handoff`: writes a note a person (or a fresh session) can read to carry an agent's work on: what happened lately and
/// what its terminal last showed, with secrets removed. Prints it, or writes it to a file with `--out`.
pub async fn handoff(agent: &str, out: Option<std::path::PathBuf>) -> Result<(), String> {
    let get = |terminal: bool, lines: usize| {
        let agent = agent.to_string();
        async move {
            let r = expect_ok(
                ipc::call(
                    &home_dir(),
                    &Req::Logs {
                        agent,
                        lines,
                        terminal,
                    },
                )
                .await
                .map_err(|_| "no daemon is running here".to_string())?,
            )?;
            Ok::<Vec<String>, String>(
                r.data
                    .and_then(|d| d["lines"].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|l| l.as_str().map(String::from))
                    .collect(),
            )
        }
    };
    let events = get(false, 200).await?;
    let screen = get(true, 60).await?;
    let note = format!(
        "# Handoff: {agent}\n\n## What happened (newest last)\n\n{}\n\n## What its terminal last showed\n\n```\n{}\n```\n",
        events.join("\n"),
        screen.join("\n")
    );
    match out {
        Some(p) => {
            std::fs::write(&p, note).map_err(|e| e.to_string())?;
            println!("wrote {}", p.display());
        }
        None => print!("{note}"),
    }
    Ok(())
}

/// `claudecord attach` with or without a name: without one, lists what runs here and lets the person choose by number or name.
pub async fn attach_or_pick(agent: Option<String>) -> Result<(), String> {
    if let Some(a) = agent {
        return attach(&a).await;
    }
    let r = ipc::call(&home_dir(), &Req::List)
        .await
        .map_err(|_| "nothing is running here (the daemon is off)".to_string())?;
    let rows = r
        .data
        .and_then(|d| d.as_array().cloned())
        .unwrap_or_default();
    pick_and_attach(&rows, false).await
}

/// `claudecord start` with nothing typed after it: every option at its default.
fn default_start_args() -> StartArgs {
    #[derive(clap::Parser)]
    struct Defaults {
        #[command(flatten)]
        a: StartArgs,
    }
    <Defaults as clap::Parser>::parse_from(["start"]).a
}

/// The agents the daemon here has (none if it is off). The daemon is never started for this.
async fn list_agents() -> Vec<serde_json::Value> {
    ipc::call(&home_dir(), &Req::List)
        .await
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.as_array().cloned())
        .unwrap_or_default()
}

/// Lists `rows` to choose from and opens the one chosen; each line says which command it runs. The agents working in this folder come first
/// and the first line is highlighted, so Enter does the likely thing. With `with_new` the list ends with the ways to start an agent, and
/// when no agent works in this folder the highlight goes to "new agent in this folder".
async fn pick_and_attach(rows: &[serde_json::Value], with_new: bool) -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(format!("name one: {} attach NAME", me()));
    }
    if rows.is_empty() && !with_new {
        return Err("nothing is running here".into());
    }
    let here = std::env::current_dir().unwrap_or_default();
    let mut rows = rows.to_vec();
    let mine = |a: &serde_json::Value| a["cwd"].as_str().is_some_and(|c| here.starts_with(c));
    rows.sort_by_key(|a| !mine(a));
    let any_here = rows.first().is_some_and(mine);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|a| a["agent"].as_str().map(String::from))
        .collect();
    let mut labels: Vec<String> = rows
        .iter()
        .map(|a| {
            format!(
                "{:26} {:9} {:24} attach",
                a["agent"].as_str().unwrap_or(""),
                a["status"].as_str().unwrap_or(""),
                {
                    // A long folder keeps its end (the part that tells folders apart).
                    let c = a["cwd"].as_str().unwrap_or("");
                    let n = c.chars().count();
                    if n > 24 {
                        format!("…{}", c.chars().skip(n - 23).collect::<String>())
                    } else {
                        c.to_string()
                    }
                }
            )
        })
        .collect();
    if with_new {
        labels.push(format!(
            "{:56} start --pick",
            "+ new agent (choose project, name, program on the dashboard)"
        ));
    }
    let first = if any_here { 0 } else { names.len() };
    let title = if with_new {
        "claudecord"
    } else {
        "Open which agent?"
    };
    let picked = tokio::task::spawn_blocking(move || pick_with_keys(title, &labels, first))
        .await
        .map_err(|e| e.to_string())??;
    match picked {
        Some(i) if i < names.len() => attach(&names[i]).await,
        Some(_) => {
            // Everything about the new agent (project, name, program, role) is chosen on the dashboard, so nothing is typed here twice.
            let mut a = default_start_args();
            a.pick = true;
            start(a).await
        }
        None => Ok(()),
    }
}

/// The first line of a window of `visible` lines, out of `len`, that keeps line `sel` in view.
fn window_top(sel: usize, len: usize, visible: usize) -> usize {
    sel.saturating_sub(visible / 2)
        .min(len.saturating_sub(visible))
}

/// `text` cut to fit `max` terminal columns (a wide character, such as Chinese or an emoji, takes two), ending in an ellipsis when cut.
fn cut_to_width(text: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// What a key press does in the list.
#[derive(Debug, PartialEq)]
enum PickKey {
    /// Move the highlight to this line.
    Move(usize),
    /// Open this line.
    Open(usize),
    Cancel,
    Nothing,
}

/// The meaning of a key in a list of `len` lines with line `sel` highlighted: the arrow keys (or j and k) move, a digit opens that line at once,
/// Enter opens the highlighted one, Esc, q and Ctrl-C leave.
fn on_key(
    code: crossterm::event::KeyCode,
    mods: crossterm::event::KeyModifiers,
    sel: usize,
    len: usize,
) -> PickKey {
    use crossterm::event::{KeyCode, KeyModifiers};
    match code {
        KeyCode::Up | KeyCode::Char('k') => PickKey::Move(if sel == 0 { len - 1 } else { sel - 1 }),
        KeyCode::Down | KeyCode::Char('j') => PickKey::Move((sel + 1) % len),
        KeyCode::Home => PickKey::Move(0),
        KeyCode::End => PickKey::Move(len - 1),
        KeyCode::Enter => PickKey::Open(sel),
        KeyCode::Esc | KeyCode::Char('q') => PickKey::Cancel,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => PickKey::Cancel,
        KeyCode::Char(d) if d.is_ascii_digit() && d != '0' && (d as usize - '1' as usize) < len => {
            PickKey::Open(d as usize - '1' as usize)
        }
        _ => PickKey::Nothing,
    }
}

/// Shows `labels` as a list to go through with the keys (see `on_key`) and returns the line chosen, or None if the person left. The list is
/// taken off the screen again afterwards.
fn pick_with_keys(title: &str, labels: &[String], start: usize) -> Result<Option<usize>, String> {
    use crossterm::event::{self, Event, KeyEventKind};
    use crossterm::style::{Attribute, Print, SetAttribute};
    use crossterm::terminal::{Clear, ClearType};
    use crossterm::{cursor, queue};
    use std::io::Write;
    if labels.is_empty() {
        return Ok(None);
    }
    // A terminal that does not say how wide it is (0) counts as 80 wide.
    let width = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .ok()
        .filter(|c| *c >= 20)
        .unwrap_or(80);
    // As many lines as fit above the cursor with the title and a spare line (at least 3): a longer list scrolls with the highlight.
    let height = crossterm::terminal::size()
        .map(|(_, r)| r as usize)
        .ok()
        .filter(|r| *r >= 5)
        .unwrap_or(24);
    let visible = labels.len().min(height.saturating_sub(3).max(3));
    let shown: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let n = if i < 9 {
                format!("{}", i + 1)
            } else {
                " ".into()
            };
            cut_to_width(&format!(" {n}  {l}"), width.saturating_sub(3))
        })
        .collect();
    let mut out = std::io::stdout();
    let _raw = RawMode::enter();
    let draw = |sel: usize, again: bool, out: &mut std::io::Stdout| -> std::io::Result<()> {
        if again {
            queue!(
                out,
                cursor::MoveUp(visible as u16 + 1),
                cursor::MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        let top = window_top(sel, labels.len(), visible);
        let more = if visible < labels.len() {
            format!("  [{}/{}]", sel + 1, labels.len())
        } else {
            String::new()
        };
        // The title is cut like the lines: if it wrapped, the redraw would move up by the wrong number of lines.
        let head = cut_to_width(
            &format!("{title}  (arrows or 1-9, Enter opens, Esc cancels){more}"),
            width.saturating_sub(1),
        );
        queue!(out, Print(format!("{head}\r\n")))?;
        for (i, l) in shown.iter().enumerate().skip(top).take(visible) {
            if i == sel {
                queue!(
                    out,
                    SetAttribute(Attribute::Reverse),
                    Print(format!("\u{276F}{l}")),
                    SetAttribute(Attribute::Reset),
                    Print("\r\n")
                )?;
            } else {
                queue!(out, Print(format!(" {l}\r\n")))?;
            }
        }
        out.flush()
    };
    let mut sel = start.min(labels.len() - 1);
    queue!(out, cursor::Hide).map_err(|e| e.to_string())?;
    draw(sel, false, &mut out).map_err(|e| e.to_string())?;
    let chosen = loop {
        let Event::Key(k) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match on_key(k.code, k.modifiers, sel, labels.len()) {
            PickKey::Move(n) => {
                sel = n;
                draw(sel, true, &mut out).map_err(|e| e.to_string())?;
            }
            PickKey::Open(n) => break Some(n),
            PickKey::Cancel => break None,
            PickKey::Nothing => {}
        }
    };
    let _ = queue!(
        out,
        cursor::MoveUp(visible as u16 + 1),
        cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        cursor::Show
    );
    let _ = out.flush();
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn the_list_moves_with_the_arrows_opens_with_a_digit_or_enter_and_leaves_with_escape() {
        let k = |c, sel| on_key(c, KeyModifiers::NONE, sel, 3);
        assert_eq!(k(KeyCode::Down, 0), PickKey::Move(1));
        assert_eq!(
            k(KeyCode::Down, 2),
            PickKey::Move(0),
            "past the end it goes round"
        );
        assert_eq!(k(KeyCode::Up, 0), PickKey::Move(2));
        assert_eq!(k(KeyCode::Char('j'), 0), PickKey::Move(1));
        assert_eq!(k(KeyCode::Char('2'), 0), PickKey::Open(1));
        assert_eq!(
            k(KeyCode::Char('4'), 0),
            PickKey::Nothing,
            "there is no fourth line"
        );
        assert_eq!(k(KeyCode::Char('0'), 0), PickKey::Nothing);
        assert_eq!(k(KeyCode::Enter, 2), PickKey::Open(2));
        assert_eq!(k(KeyCode::Esc, 1), PickKey::Cancel);
        assert_eq!(
            on_key(KeyCode::Char('c'), KeyModifiers::CONTROL, 0, 3),
            PickKey::Cancel
        );
        assert_eq!(k(KeyCode::Char('x'), 0), PickKey::Nothing);
    }
}

const GUIDE_START: &str = "<!-- claudecord:start -->";
const GUIDE_END: &str = "<!-- claudecord:end -->";

/// `claudecord init`: the guide for agents goes into AGENTS.md here (and, with `--claude`, CLAUDE.md here points at it). Only the part between the
/// markers is ever replaced; anything else in either file is left as it is.
pub async fn init(claude: bool) -> Result<(), String> {
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    for line in install_guide(&dir, claude)? {
        println!("{line}");
    }
    Ok(())
}

/// The files an agent reads to learn about the project, in the order they are looked for.
const INSTRUCTION_FILES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    ".github/copilot-instructions.md",
];

/// Puts the guide into every instruction file that is already in the folder (appended after what is there; only the part between the markers is ever
/// replaced), or makes AGENTS.md if there is none. `claude` also makes sure CLAUDE.md has it, even if that file was not there. Returns what was done,
/// one line each.
fn install_guide(dir: &std::path::Path, claude: bool) -> Result<Vec<String>, String> {
    let block = format!(
        "{GUIDE_START}\n{}{GUIDE_END}\n",
        crate::agents::AGENTS_GUIDE
    );
    let mut targets: Vec<&str> = INSTRUCTION_FILES
        .iter()
        .copied()
        .filter(|f| dir.join(f).is_file())
        .collect();
    if claude && !targets.contains(&"CLAUDE.md") {
        targets.push("CLAUDE.md");
    }
    if targets.is_empty() {
        targets.push("AGENTS.md");
    }
    let mut done = Vec::new();
    for f in targets {
        let path = dir.join(f);
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let new = match (old.find(GUIDE_START), old.find(GUIDE_END)) {
            (Some(a), Some(b)) if a < b => format!(
                "{}{}{}",
                &old[..a],
                block,
                old[b + GUIDE_END.len()..].trim_start_matches('\n')
            ),
            _ if old.is_empty() => block.clone(),
            _ => format!("{}\n\n{block}", old.trim_end()),
        };
        if new != old {
            std::fs::write(&path, new).map_err(|e| e.to_string())?;
            done.push(format!("wrote the team guide to {f}"));
        } else {
            done.push(format!("{f} already has the current guide"));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod guide_tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cc-guide-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_guide_is_added_to_the_instruction_files_that_exist_updated_in_place_and_never_overwrites_the_users_own_text()
     {
        let d = dir("existing");
        std::fs::write(d.join("AGENTS.md"), "# My project\nuse tabs\n").unwrap();
        std::fs::write(d.join("CLAUDE.md"), "Be terse.\n").unwrap();
        let lines = install_guide(&d, false).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        for f in ["AGENTS.md", "CLAUDE.md"] {
            let t = std::fs::read_to_string(d.join(f)).unwrap();
            assert_eq!(t.matches(GUIDE_START).count(), 1, "{f}");
        }
        let a = std::fs::read_to_string(d.join("AGENTS.md")).unwrap();
        assert!(
            a.starts_with("# My project\nuse tabs"),
            "their text stays first"
        );
        assert!(
            std::fs::read_to_string(d.join("CLAUDE.md"))
                .unwrap()
                .starts_with("Be terse.")
        );
        assert!(
            !d.join("GEMINI.md").exists(),
            "a file that was not there is not made"
        );
        // Again: nothing changes. After an edit inside the markers: put back; text outside them is kept.
        install_guide(&d, false).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("AGENTS.md")).unwrap(), a);
        let edited = a.replace("claudecord team", "claudecord XXXX") + "\nafter\n";
        std::fs::write(d.join("AGENTS.md"), &edited).unwrap();
        install_guide(&d, false).unwrap();
        let b = std::fs::read_to_string(d.join("AGENTS.md")).unwrap();
        assert!(b.contains("claudecord guide") && !b.contains("XXXX") && b.ends_with("after\n"));
        assert_eq!(b.matches(GUIDE_START).count(), 1);
    }

    #[test]
    fn with_no_instruction_file_agents_md_is_made_and_claude_md_only_when_asked() {
        let d = dir("none");
        install_guide(&d, false).unwrap();
        assert!(d.join("AGENTS.md").exists() && !d.join("CLAUDE.md").exists());
        install_guide(&d, true).unwrap();
        assert!(
            std::fs::read_to_string(d.join("CLAUDE.md"))
                .unwrap()
                .contains(GUIDE_START)
        );
    }
}

/// What to type to run this program again. A person who ran it with `npx claudecord` has no `claudecord` command in their shell, so the
/// launcher says how it was started (`CLAUDECORD_RUN`) and the hints repeat that instead of a command that would not be found.
fn me() -> String {
    std::env::var("CLAUDECORD_RUN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claudecord".into())
}

/// Whether the daemon still has this agent.
async fn still_running(agent: &str) -> bool {
    ipc::call(&home_dir(), &Req::List)
        .await
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.as_array().cloned())
        .is_some_and(|l| l.iter().any(|a| a["agent"].as_str() == Some(agent)))
}

/// `claudecord settings`: shows the settings, or changes one.
pub async fn settings(name: Option<String>, value: Option<String>) -> Result<(), String> {
    let dir = home_dir();
    match (name.as_deref(), value.as_deref()) {
        (None, _) => {}
        (Some("keep-running"), Some(v)) => {
            let on = match v {
                "on" | "yes" | "true" => true,
                "off" | "no" | "false" => false,
                _ => return Err("keep-running is on or off".into()),
            };
            crate::device::config::set_keep_running(&dir, on).map_err(|e| e.to_string())?;
        }
        (Some("keep-running"), None) => {}
        (Some(other), _) => {
            return Err(format!("no setting called {other}. There is: keep-running"));
        }
    }
    let on = crate::device::config::keep_running(&dir);
    println!(
        "keep-running: {}",
        if on {
            "on (this machine stays connected to the hub even when no agent is running)"
        } else {
            "off (this machine disconnects 20 seconds after the last agent ends, and reconnects when you start one)"
        }
    );
    println!("change it with: {} settings keep-running on|off", me());
    Ok(())
}

/// Lists `rows` and asks whether to stop them all. Anything but y is no; with no terminal to ask on it is yes (a script said `stop`).
fn confirm(rows: &[serde_json::Value]) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return true;
    }
    println!("Running here:");
    for a in rows {
        println!(
            "  {:28} {}",
            a["agent"].as_str().unwrap_or(""),
            a["cwd"].as_str().unwrap_or("")
        );
    }
    print!(
        "Stop {} agent(s) and disconnect this machine? [y/N] ",
        rows.len()
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("y")
}

#[cfg(test)]
mod picker_tests {
    use super::{cut_to_width, update_command, window_top};

    #[test]
    fn the_update_command_matches_how_it_was_installed() {
        assert_eq!(
            update_command("/Users/a/.local/share/uv/tools/claudecord/bin/claudecord"),
            "uv tool upgrade claudecord"
        );
        assert_eq!(
            update_command(
                "/home/a/.nvm/versions/node/v24/lib/node_modules/claudecord/bin/linux-x64/claudecord"
            ),
            "npm i -g claudecord@latest"
        );
        assert!(update_command("/usr/local/bin/claudecord").starts_with("pip install -U"));
        assert!(
            update_command(
                "C:\\Users\\a\\AppData\\Roaming\\uv\\tools\\claudecord\\Scripts\\claudecord.exe"
            )
            .starts_with("uv tool")
        );
    }

    #[test]
    fn a_list_line_fits_the_columns_and_a_long_list_scrolls_with_the_highlight() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(cut_to_width("short", 10), "short");
        for t in [
            "abcdefghijklmnop",
            "日本語日本語日本語日本語",
            "a😀b😀c😀d😀e😀f😀",
        ] {
            for max in [4, 7, 10] {
                assert!(cut_to_width(t, max).width() <= max, "{t} {max}");
            }
        }
        // 30 lines, 8 showing: the highlight is always among them, at the first, the middle and the last line.
        for sel in [0, 1, 14, 28, 29] {
            let top = window_top(sel, 30, 8);
            assert!(top <= sel && sel < top + 8 && top + 8 <= 30, "{sel} {top}");
        }
    }
}
