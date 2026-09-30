//! Commands for a machine that runs agents: save the hub address, run the daemon, start an agent in the current folder,
//! open an agent's terminal, list, stop, and check connectivity. The daemon is started for you the first time it is needed.

use crate::device::config::{Config, home_dir};
use crate::device::daemon::{self, Options};
use crate::device::doctor;
use crate::device::ipc::{self, Req, Resp};
use crate::device::link::LinkOpts;
use clap::Args;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Args)]
pub struct LoginArgs {
    /// Where the hub is, for example wss://hub.example.com
    #[arg(long)]
    pub hub: String,
    /// The token the hub gave this machine.
    #[arg(long)]
    pub token: String,
    /// A name for this machine (default: its hostname).
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct UpArgs {
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
}

/// Saves the hub address and token for this machine.
pub fn login(a: LoginArgs) -> Result<(), String> {
    let name = a.name.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "this-machine".into())
    });
    let cfg = Config {
        hub_url: a.hub,
        token: a.token,
        node_name: name,
    };
    cfg.save(&home_dir()).map_err(|e| e.to_string())?;
    println!("saved. Next: claudecord doctor");
    Ok(())
}

/// Runs the daemon in the foreground.
pub async fn run_daemon() -> Result<(), String> {
    let dir = home_dir();
    let cfg = Config::load(&dir).ok_or("not logged in: run claudecord login first")?;
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
        return Err("not logged in: run claudecord login first".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = std::fs::File::create(dir.join("daemon.log")).map_err(|e| e.to_string())?;
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

/// Starts an agent in the current folder, then opens its terminal unless asked not to.
pub async fn up(a: UpArgs) -> Result<(), String> {
    ensure_daemon().await?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
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
            name: a.name,
            adapter: a.adapter,
            model: a.model,
            role: a.role,
            cwd: cwd.to_string_lossy().into(),
            policy: a.policy,
            rows,
            cols,
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

/// The size of the person's terminal as (columns, rows), via `stty`, or a default.
fn terminal_size() -> (u16, u16) {
    let out = Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
        .ok();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut it = text
        .split_whitespace()
        .filter_map(|n| n.parse::<u16>().ok());
    match (it.next(), it.next()) {
        (Some(rows), Some(cols)) => (cols, rows),
        _ => (100, 30),
    }
}

/// Puts the person's terminal in raw mode and restores it when dropped, even if the program ends early.
struct RawMode;

impl RawMode {
    fn enter() -> Self {
        let _ = Command::new("stty")
            .args(["raw", "-echo"])
            .stdin(std::process::Stdio::inherit())
            .status();
        RawMode
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = Command::new("stty")
            .arg("sane")
            .stdin(std::process::Stdio::inherit())
            .status();
    }
}

/// Opens an agent's terminal in this window. Keys go to the agent, its output comes back, and the window size follows.
/// Close the window or press Ctrl-] to leave; the agent keeps running.
pub async fn attach(agent: &str) -> Result<(), String> {
    let sock: PathBuf = ipc::socket_path(&home_dir());
    let mut s = UnixStream::connect(&sock)
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
    let (rd, mut wr) = s.into_split();
    let mut rd = BufReader::new(rd);
    let mut reply = String::new();
    rd.read_line(&mut reply).await.map_err(|e| e.to_string())?;
    let resp: Resp = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    expect_ok(resp)?;
    let _raw = RawMode::enter();
    let (cols, rows) = terminal_size();
    let mut frame = vec![1u8, 0, 4];
    frame.extend_from_slice(&rows.to_be_bytes());
    frame.extend_from_slice(&cols.to_be_bytes());
    let _ = wr.write_all(&frame).await;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|e| e.to_string())?;
    let mut inbuf = [0u8; 1024];
    let mut outbuf = [0u8; 8192];
    loop {
        tokio::select! {
            n = stdin.read(&mut inbuf) => {
                let n = n.map_err(|e| e.to_string())?;
                if n == 0 || inbuf[..n].contains(&0x1d) { break; }
                let mut f = vec![0u8];
                f.extend_from_slice(&(n as u16).to_be_bytes());
                f.extend_from_slice(&inbuf[..n]);
                if wr.write_all(&f).await.is_err() { break; }
            }
            n = rd.read(&mut outbuf) => {
                let n = n.map_err(|e| e.to_string())?;
                if n == 0 { break; }
                let _ = stdout.write_all(&outbuf[..n]).await;
                let _ = stdout.flush().await;
            }
            _ = winch.recv() => {
                let (cols, rows) = terminal_size();
                let mut f = vec![1u8, 0, 4];
                f.extend_from_slice(&rows.to_be_bytes());
                f.extend_from_slice(&cols.to_be_bytes());
                let _ = wr.write_all(&f).await;
            }
        }
    }
    println!("\r\nleft {agent}; it is still running (claudecord attach {agent} to return)");
    Ok(())
}
