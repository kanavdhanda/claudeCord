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
    /// Run this command instead of the agent program (anything that runs in a terminal). Put it after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// Joins a hub: with a token, saves it; without one, opens the browser to sign in with Discord and approve this machine.
pub async fn login(a: LoginArgs) -> Result<(), String> {
    let hub = a
        .hub
        .or_else(|| std::env::var("CLAUDECORD_HUB").ok())
        .unwrap_or_else(|| enroll::DEFAULT_HUB.to_string());
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
    println!("saved. Next: claudecord start (in a project folder)");
    Ok(())
}

/// What bare `claudecord` (and `npx claudecord`) does. A machine that has never joined opens the browser to sign in and approve it; one
/// that has says where it is connected and what to do next.
pub async fn home() -> Result<(), String> {
    if let Some(cfg) = Config::load(&home_dir()) {
        println!("Connected to {} as {}.", cfg.hub_url, cfg.node_name);
        // The daemon is never started just to look: if it is not running, nothing is.
        match ipc::call(&home_dir(), &Req::List).await {
            Ok(r) if r.ok => {
                let rows = r
                    .data
                    .and_then(|d| d.as_array().cloned())
                    .unwrap_or_default();
                println!("\nRunning here ({}):", rows.len());
                for a in &rows {
                    println!(
                        "  {:28} {:12} {}",
                        a["agent"].as_str().unwrap_or(""),
                        a["status"].as_str().unwrap_or(""),
                        a["cwd"].as_str().unwrap_or("")
                    );
                }
                if !rows.is_empty() {
                    println!("Open one:  claudecord attach NAME");
                }
            }
            _ => println!("\nNothing is running here (the daemon is off)."),
        }
        println!("\nStart an agent in a project folder:  claudecord start");
        println!("Everything else:                      claudecord --help");
        return Ok(());
    }
    login(LoginArgs {
        hub: None,
        token: None,
        name: None,
    })
    .await?;
    println!(
        "Connected. Open the dashboard to pick where your project lives, then run `claudecord start` in a project folder."
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
    let cfg = Config::load(&dir).ok_or("not logged in: run claudecord login first")?;
    // The daemon logs to the terminal, which `claudecord start` points at daemon.log when it starts the daemon for you.
    crate::log::init(None);
    daemon::run(cfg, dir, Options::default())
        .await
        .map_err(|e| e.to_string())
}

/// Makes sure a daemon is listening, starting one in the background if not.
async fn ensure_daemon() -> Result<(), String> {
    let dir = home_dir();
    if ipc::call(&dir, &Req::Ping).await.is_ok() {
        return Ok(());
    }
    if Config::load(&dir).is_none() {
        // A machine that has never joined: this is the first run, so sign in through the browser now rather than failing.
        login(LoginArgs {
            hub: None,
            token: None,
            name: None,
        })
        .await?;
    }
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

/// `claudecord start`: starts the daemon if it is not running (and says so), starts an agent in the current folder, and opens its
/// terminal unless asked not to. It does exactly what was asked and nothing more: no handoff, no restarts, no shared folders.
pub async fn start(a: StartArgs) -> Result<(), String> {
    ensure_daemon().await?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let name = match a.name.clone() {
        Some(n) => Some(n),
        None => ask_name()?,
    };
    let project = a.project.unwrap_or_else(|| {
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into())
    });
    let (cols, rows) = terminal_size();
    let r = ipc::call(
        &home_dir(),
        &Req::Up {
            project,
            name,
            adapter: a.adapter,
            model: a.model,
            role: a.role,
            cwd: cwd.to_string_lossy().into(),
            policy: a.policy,
            rows,
            cols,
            opts: UpOpts {
                command: (!a.command.is_empty()).then_some(a.command),
                worktree: a.worktree,
                pickup: a.pickup,
                restart: a.restart,
            },
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let agent = expect_ok(r)?.msg;
    println!("started {agent}");
    if a.detach {
        println!("attach later with: claudecord attach {agent}");
        return Ok(());
    }
    attach(&agent).await
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

/// Stops one agent.
pub async fn stop(agent: &str) -> Result<(), String> {
    expect_ok(
        ipc::call(
            &home_dir(),
            &Req::Stop {
                agent: agent.into(),
            },
        )
        .await
        .map_err(|e| e.to_string())?,
    )?;
    println!("stopped {agent}");
    Ok(())
}

/// Stops the daemon, which stops every agent.
pub async fn down() -> Result<(), String> {
    match ipc::call(&home_dir(), &Req::Shutdown).await {
        Ok(_) => println!("stopped"),
        Err(_) => println!("no daemon was running"),
    }
    Ok(())
}

/// Checks whether this machine can reach the hub and prints each step.
pub async fn doctor() -> Result<(), String> {
    let cfg = Config::load(&home_dir()).ok_or("not logged in: run claudecord login first")?;
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
        return if status.success() {
            Ok(())
        } else {
            Err(format!("{} ended with {status}", cmd[0]))
        };
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
    println!("\r\nleft {agent}; it is still running (claudecord attach {agent} to return)");
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

/// Asks what to call the agent, when a person is at the keyboard. Enter alone keeps the random friendly name.
fn ask_name() -> Result<Option<String>, String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    loop {
        print!("Name for this agent (Enter for a random one): ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let n = line.trim();
        if n.is_empty() {
            return Ok(None);
        }
        if crate::protocol::is_slug(n) {
            return Ok(Some(n.to_string()));
        }
        println!("Names use letters, digits, dots, dashes and underscores.");
    }
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
    let names: Vec<String> = rows
        .iter()
        .filter_map(|a| a["agent"].as_str().map(String::from))
        .collect();
    match names.as_slice() {
        [] => return Err("nothing is running here".into()),
        [only] => return attach(only).await,
        _ => {}
    }
    use std::io::{IsTerminal, Write};
    for (i, a) in rows.iter().enumerate() {
        println!(
            "{:>2}) {:28} {:12} {}",
            i + 1,
            a["agent"].as_str().unwrap_or(""),
            a["status"].as_str().unwrap_or(""),
            a["cwd"].as_str().unwrap_or("")
        );
    }
    if !std::io::stdin().is_terminal() {
        return Err("name one: claudecord attach NAME".into());
    }
    loop {
        print!("Open which? (number or name, Enter to cancel): ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            return Ok(());
        }
        match choose(&names, &line) {
            Some(a) => return attach(&a).await,
            None => println!("No match. Type a number from the list, or a name."),
        }
    }
}

/// What the person typed at the list: a number, a full name, the short name after the project, or the start of a name that fits only one.
fn choose(names: &[String], input: &str) -> Option<String> {
    let t = input.trim();
    if let Ok(n) = t.parse::<usize>() {
        return names.get(n.checked_sub(1)?).cloned();
    }
    let hits: Vec<&String> = names
        .iter()
        .filter(|a| a.as_str() == t || a.rsplit('/').next() == Some(t))
        .collect();
    let hits = if hits.is_empty() {
        names.iter().filter(|a| a.starts_with(t)).collect()
    } else {
        hits
    };
    (hits.len() == 1).then(|| hits[0].clone())
}

#[cfg(test)]
mod tests {
    use super::choose;

    #[test]
    fn the_list_takes_a_number_a_name_or_a_unique_start() {
        let n: Vec<String> = ["demo/otter", "demo/heron", "web/otter"]
            .map(String::from)
            .to_vec();
        assert_eq!(choose(&n, "2").as_deref(), Some("demo/heron"));
        assert_eq!(choose(&n, "web/otter").as_deref(), Some("web/otter"));
        assert_eq!(choose(&n, "heron").as_deref(), Some("demo/heron"));
        assert_eq!(choose(&n, "otter"), None, "two agents are called otter");
        assert_eq!(choose(&n, "de").as_deref(), None, "a start that fits two");
        assert_eq!(choose(&n, "0"), None);
        assert_eq!(choose(&n, "9"), None);
    }
}
