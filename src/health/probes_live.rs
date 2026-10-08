//! Health probes for the running pieces: the hub server, the device's link and terminals, the daemon with a stand-in
//! agent, and the command line. These start real servers on a free local port, real pseudo-terminals and real sockets,
//! so they prove the pieces are connected, not just present.

use super::{Feature, Probe, boxed, eventually, scratch};
use crate::device::config::Config;
use crate::device::daemon::{self, Options};
use crate::device::doctor;
use crate::device::inject::{Guard, Wait};
use crate::device::ipc::{self, Req};
use crate::device::link::{self, LinkEvent, LinkOpts};
use crate::hub::*;
use crate::server::{self, Config as ServerConfig};
use crate::store::Store;
use crate::sync::Lock;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

macro_rules! ensure {
    ($c:expr, $($m:tt)*) => {
        if !$c {
            return Err(format!($($m)*));
        }
    };
}

/// A stand-in for the `claude` program: prints the ready line, then echoes each line it is given into ./fake.log.
const NORMAL: &str = "#!/bin/sh\necho run >> starts.log\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";
/// The same, but the first start dies at once.
const DIES_ONCE: &str = "#!/bin/sh\necho run >> starts.log\nif [ ! -f started ]; then touch started; exit 1; fi\necho 'fake claude'\nprintf '? for shortcuts\\n'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> fake.log\n  echo \"ack: $line\"\n  printf '? for shortcuts\\n'\ndone\n";

fn kd() -> Human {
    Human {
        id: "1".into(),
        name: "kd".into(),
    }
}

fn server_cfg(bind: &str) -> ServerConfig {
    ServerConfig {
        bind: bind.parse().expect("address"),
        ping_every: Duration::from_millis(100),
        tick_every: Duration::from_millis(50),
        ..ServerConfig::default()
    }
}

fn link_opts() -> LinkOpts {
    LinkOpts {
        ping_every: Duration::from_millis(100),
        connect_timeout: Duration::from_secs(10),
        backoff_min: Duration::from_millis(30),
        backoff_max: Duration::from_millis(300),
        proxy: None,
    }
}

/// A hub with a token for machine "mac". Returns the hub, its token and the database path.
async fn hub(dir: &Path, bind: &str) -> Result<(server::Hub, String, PathBuf), String> {
    let db = dir.join("hub.db");
    let mut store = Store::open(&db, None).map_err(|e| e.to_string())?;
    let token = store.create_token("mac", 0).map_err(|e| e.to_string())?;
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(server_cfg(bind), core, store)
        .await
        .map_err(|e| e.to_string())?;
    Ok((hub, token, db))
}

/// The features checked here.
pub fn features() -> Vec<Feature> {
    vec![
        Feature {
            name: "hub server and device connections",
            covers: &["server"],
            probe: server_probe,
        },
        Feature {
            name: "device link reconnects by itself",
            covers: &["device/link", "device/config"],
            probe: reconnect,
        },
        Feature {
            name: "connection check",
            covers: &["device/doctor"],
            probe: doctor_probe,
        },
        Feature {
            name: "terminals and safe pasting",
            covers: &["device/pty", "device/inject", "device/terminal"],
            probe: terminals,
        },
        Feature {
            name: "tmux sessions: start, paste, read, log, stop",
            covers: &["device/tmux", "device/logs", "device/shot"],
            probe: tmux_probe,
        },
        Feature {
            name: "daemon: message in, agent speaks, ask, file",
            covers: &["device/daemon", "device/ipc", "device/mod"],
            probe: daemon_flow,
        },
        Feature {
            name: "logging, supervision and locks that survive a panic",
            covers: &["log", "task", "sync", "notify"],
            probe: robustness_probe,
        },
        Feature {
            name: "uptime: recorded, ready check, outside prober",
            covers: &["uptime"],
            probe: uptime_probe,
        },
        Feature {
            name: "dashboard sign-in with Discord (against a stand-in)",
            covers: &["server/login"],
            probe: signin_probe,
        },
        Feature {
            name: "dashboard in a browser",
            covers: &["server"],
            probe: dashboard_probe,
        },
        Feature {
            name: "discord bridge (against a stand-in Discord)",
            covers: &[
                "discord/api",
                "discord/gateway",
                "discord/bridge",
                "discord/commands",
                "discord/fake",
            ],
            probe: discord_probe,
        },
        Feature {
            name: "old history in a bucket (Oracle, R2, S3)",
            covers: &["store"],
            probe: bucket_probe,
        },
        Feature {
            name: "Obsidian export",
            covers: &["export"],
            probe: obsidian_probe,
        },
        Feature {
            name: "automatic history rollover",
            covers: &["store", "server"],
            probe: rollover_probe,
        },
        Feature {
            name: "agent restart after a crash",
            covers: &["device/daemon"],
            probe: restart,
        },
        Feature {
            name: "command line",
            covers: &["cli"],
            probe: command_line,
        },
    ]
}

fn server_probe() -> Probe {
    boxed(async {
        let dir = scratch("server");
        let (hub, token, db) = hub(&dir, "127.0.0.1:0").await?;
        let url = format!("ws://{}/api/v1/node/connect", hub.addr);
        let bad = link::connect_detailed(&url, "wrong", &link_opts()).await;
        ensure!(
            matches!(&bad, Err(e) if e.contains("401")),
            "a wrong token was not refused"
        );
        let (tx, mut rx) = mpsc::channel(16);
        let l = link::spawn(url, token, "mac".into(), tx, link_opts());
        let up = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(e) = rx.recv().await {
                if e == LinkEvent::Up {
                    return true;
                }
            }
            false
        })
        .await;
        ensure!(
            up == Ok(true),
            "a device with a good token was not welcomed"
        );
        l.send(crate::protocol::NodeFrame::AgentRegister {
            agent: crate::protocol::AgentSpec {
                agent_id: "p/a".into(),
                name: "a".into(),
                project: "p".into(),
                adapter: crate::protocol::AdapterId::Claude,
                model: None,
                role: None,
            },
            cwd: "/x".into(),
        })
        .await;
        let listed = eventually(async || {
            hub.devices()
                .await
                .first()
                .is_some_and(|d| d.connected && d.agents.len() == 1)
        })
        .await;
        ensure!(
            listed,
            "the device list does not show the connected machine and its agent"
        );
        let first = hub.devices().await[0].last_seen.unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(350)).await;
        ensure!(
            hub.devices().await[0].last_seen.unwrap_or(0) > first,
            "heartbeats are not counted as signs of life"
        );
        let saved = eventually(async || {
            Store::open(&db, None)
                .ok()
                .and_then(|s| s.load_state().ok())
                .is_some_and(|rows| rows.iter().any(|(k, _)| k == "agents:p/a"))
        })
        .await;
        ensure!(saved, "the agent was never saved to the database");
        hub.shutdown().await;
        Ok("bad token refused, good token welcomed, device listed, heartbeats counted, state saved by the timer, clean shutdown".into())
    })
}

fn reconnect() -> Probe {
    boxed(async {
        let dir = scratch("reconnect");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let bind = probe.local_addr().map_err(|e| e.to_string())?.to_string();
        drop(probe);
        let (hub1, token, db) = hub(&dir, &bind).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let _l = link::spawn(
            format!("ws://{bind}/api/v1/node/connect"),
            token,
            "mac".into(),
            tx,
            link_opts(),
        );
        async fn wait(rx: &mut mpsc::Receiver<LinkEvent>, want: LinkEvent) -> bool {
            tokio::time::timeout(Duration::from_secs(3), async {
                while let Some(e) = rx.recv().await {
                    if e == want {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false)
        }
        ensure!(wait(&mut rx, LinkEvent::Up).await, "no first connection");
        hub1.shutdown().await;
        ensure!(
            wait(&mut rx, LinkEvent::Down).await,
            "the drop was not noticed"
        );
        let hub2 = server::start(
            server_cfg(&bind),
            HubCore::default(),
            Store::open(&db, None).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
        ensure!(
            wait(&mut rx, LinkEvent::Up).await,
            "the link did not come back by itself"
        );
        hub2.shutdown().await;
        let cfg = Config {
            hub_url: "wss://h".into(),
            token: "t".into(),
            node_name: "n".into(),
        };
        cfg.save(&dir).map_err(|e| e.to_string())?;
        ensure!(Config::load(&dir) == Some(cfg), "config did not round trip");
        Ok("connected, noticed the hub going away, reconnected on its own, config saved".into())
    })
}

fn doctor_probe() -> Probe {
    boxed(async {
        let dir = scratch("doctor");
        let (hub, token, _) = hub(&dir, "127.0.0.1:0").await?;
        let cfg = Config {
            hub_url: format!("ws://{}", hub.addr),
            token,
            node_name: "mac".into(),
        };
        let checks = doctor::run(&cfg, &link_opts()).await;
        ensure!(
            checks.iter().all(|c| c.ok) && checks.len() == 4,
            "doctor found problems on a healthy setup: {checks:?}"
        );
        let nowhere = Config {
            hub_url: "ws://127.0.0.1:1".into(),
            ..cfg
        };
        let bad = doctor::run(&nowhere, &link_opts()).await;
        ensure!(
            bad.iter().any(|c| c.name == "connect" && !c.ok),
            "doctor did not name the failing step"
        );
        hub.shutdown().await;
        Ok("all four steps pass on a healthy setup, the failing step is named otherwise".into())
    })
}

fn terminals() -> Probe {
    boxed(async {
        if cfg!(windows) {
            return Ok(
                "needs a Unix shell for its stand-in program, so it runs on Linux and macOS".into(),
            );
        }
        let a = crate::device::pty::PtyTerminal::spawn(
            &["cat".into()],
            Path::new("/tmp"),
            &[],
            &[],
            24,
            80,
            Guard::with_times(0, 100, 100),
        )
        .map_err(|e| e.to_string())?;
        let now = crate::now_ms() + 10_000;
        ensure!(
            a.inject("hello terminal", now).is_ok(),
            "an idle terminal refused a paste"
        );
        ensure!(
            eventually(async || a.screen_text().contains("hello terminal")).await,
            "the pasted text never reached the terminal"
        );
        a.type_input(b"half", now + 1000)
            .map_err(|e| e.to_string())?;
        ensure!(
            a.inject("x", now + 20_000) == Err(Wait::PartialLine),
            "a half-typed line was pasted over"
        );
        a.kill();
        Ok("pasted into a real terminal, refused over a half-typed line".into())
    })
}

/// A real tmux session: started, pasted into, read, copied to a log and stopped. Passes with a note where tmux is not installed.
fn tmux_probe() -> Probe {
    boxed(async {
        use crate::device::tmux::{TmuxTerminal, stop_server};
        if !TmuxTerminal::available() {
            return Ok("tmux is not installed here, so the built-in terminal is used".into());
        }
        let socket = format!("cc-probe-{}", std::process::id());
        let log = crate::device::logs::AgentLog::new(&scratch("tmux-log"), "probe/otter");
        log.prepare();
        let t = TmuxTerminal::spawn(
            &socket,
            "probe-otter",
            "probe/otter",
            &["cat".into()],
            Path::new("/tmp"),
            &[],
            &[],
            24,
            80,
            Guard::with_times(0, 100, 100),
        )
        .map_err(|e| e.to_string())?;
        t.pipe_to(&log.terminal_path());
        let now = crate::now_ms() + 10_000;
        let pasted = t.inject("hello tmux", now).is_ok();
        let seen = eventually(async || {
            t.observe(crate::now_ms() + 100_000);
            t.screen_text().contains("hello tmux")
        })
        .await;
        let logged =
            eventually(async || log.tail_terminal(10).join(" ").contains("hello tmux")).await;
        t.kill();
        stop_server(&socket);
        ensure!(pasted, "an idle tmux session refused a paste");
        ensure!(seen, "the pasted text never showed on the tmux screen");
        ensure!(logged, "tmux did not copy the screen into the log");
        Ok("pasted into a real tmux session, read it back, logged it, stopped it".into())
    })
}

/// Starts a hub and a daemon whose agent program is a stand-in script. Returns the hub, the daemon's folder and the
/// project folder.
async fn rig(
    name: &str,
    script: &str,
    restart: u32,
) -> Result<(server::Hub, PathBuf, PathBuf, String), String> {
    let root = scratch(name);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    std::fs::write(bin.join("claude"), script).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let (hub, token, _) = hub(&root, "127.0.0.1:0").await?;
    let dir = root.join("home");
    let cfg = Config {
        hub_url: format!("ws://{}", hub.addr),
        token,
        node_name: "mac".into(),
    };
    let opts = Options {
        dev_spawn: true,
        link: link_opts(),
        tick: Duration::from_millis(40),
        accept_after: Duration::from_millis(400),
        quiet: (100, 200),
        extra_path: vec![bin],
        max_agents: 8,
        labels: vec![],
        backend: crate::device::terminal::Backend::Pty,
        auto_startup: false,
        idle_exit: None,
        idle_agents_exit: None,
    };
    let d = dir.clone();
    tokio::spawn(async move {
        let _ = daemon::run(cfg, d, opts).await;
    });
    ensure_up(&dir).await?;
    let project = root.join("demo");
    std::fs::create_dir_all(&project).map_err(|e| e.to_string())?;
    let r = ipc::call(
        &dir,
        &Req::Up {
            project: "demo".into(),
            name: Some("otter".into()),
            adapter: "claude".into(),
            model: None,
            role: None,
            cwd: project.to_string_lossy().into(),
            policy: "autonomous".into(),
            rows: 24,
            cols: 80,
            opts: crate::device::ipc::UpOpts {
                restart,
                ..Default::default()
            },
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    ensure!(r.ok, "could not start the stand-in agent: {}", r.msg);
    let key = r
        .data
        .as_ref()
        .and_then(|d| d["key"].as_str())
        .unwrap_or_default()
        .to_string();
    Ok((hub, dir, project, key))
}

/// Waits for the daemon's socket to answer.
async fn ensure_up(dir: &Path) -> Result<(), String> {
    ensure!(
        eventually(async || ipc::call(dir, &Req::Ping).await.is_ok()).await,
        "the daemon never started listening"
    );
    Ok(())
}

fn daemon_flow() -> Probe {
    boxed(async {
        if cfg!(windows) {
            return Ok(
                "needs a Unix shell for its stand-in program, so it runs on Linux and macOS".into(),
            );
        }
        let (hub, dir, project, key) = rig("daemon", NORMAL, 0).await?;
        let mut chat = hub.chat();
        ensure!(
            eventually(async || hub
                .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
                .await
                .unwrap_or(false))
            .await,
            "the agent never registered with the hub"
        );
        hub.call(|c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "demo",
                    "please start",
                    &MessageOpts {
                        reference: Some("c:1"),
                        ..Default::default()
                    },
                    now,
                )
                .map(|r| r.1)
                .unwrap_or_default();
            ((), r)
        })
        .await;
        ensure!(
            eventually(async || std::fs::read_to_string(project.join("fake.log"))
                .is_ok_and(|s| s.contains("please start")))
            .await,
            "a person's message never reached the agent's terminal"
        );
        let confirmed = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(c) = chat.recv().await {
                if matches!(c, Chat::Confirm { ref reference, .. } if reference == "c:1") {
                    return true;
                }
            }
            false
        })
        .await;
        ensure!(
            confirmed == Ok(true),
            "the message was never confirmed as accepted"
        );
        let r = ipc::call_as(
            &dir,
            Some(&key),
            &Req::Say {
                agent: "demo/otter".into(),
                text: "done it".into(),
                thread: None,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        ensure!(r.ok, "the say command was refused");
        let spoke = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(c) = chat.recv().await {
                if matches!(c, Chat::Post { ref text, .. } if text == "done it") {
                    return true;
                }
            }
            false
        })
        .await;
        ensure!(
            spoke == Ok(true),
            "what the agent said never reached the chat"
        );
        let r = ipc::call_as(
            &dir,
            Some(&key),
            &Req::Ask {
                agent: "demo/otter".into(),
                question: "which db?".into(),
                options: None,
                thread: None,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        ensure!(
            r.ok && r.msg.contains("End your turn"),
            "the ask did not return at once"
        );
        ensure!(
            eventually(async || hub
                .call(|c, _| (c.asks_of("demo").len() == 1, vec![]))
                .await
                .unwrap_or(false))
            .await,
            "the ask never reached the hub"
        );
        hub.call(|c, now| {
            let fx = c
                .answer_ask(&Answerer::Human(kd()), "demo", "Q1", "postgres", now)
                .map(|o| o.effects)
                .unwrap_or_default();
            ((), fx)
        })
        .await;
        ensure!(
            eventually(async || std::fs::read_to_string(project.join("fake.log"))
                .is_ok_and(|s| s.contains("postgres")))
            .await,
            "the answer never reached the terminal"
        );
        hub.call(|c, _| {
            let fx = c
                .send_file(&kd(), "demo", "see", "plan.txt", b"the plan", None, "t1")
                .map(|r| r.1)
                .unwrap_or_default();
            ((), fx)
        })
        .await;
        ensure!(
            eventually(
                async || std::fs::read(project.join(".claudecord/files/demo/plan.txt"))
                    .is_ok_and(|b| b == b"the plan")
            )
            .await,
            "a file sent from chat never landed in the inbox"
        );
        let _ = ipc::call(&dir, &Req::Shutdown).await;
        hub.shutdown().await;
        Ok("message pasted and confirmed, say reached the chat, ask answered, file saved to the inbox".into())
    })
}

fn restart() -> Probe {
    boxed(async {
        if cfg!(windows) {
            return Ok(
                "needs a Unix shell for its stand-in program, so it runs on Linux and macOS".into(),
            );
        }
        let (hub, dir, project, _key) = rig("restart", DIES_ONCE, 3).await?;
        ensure!(
            eventually(async || std::fs::read_to_string(project.join("starts.log"))
                .is_ok_and(|s| s.lines().count() >= 2))
            .await,
            "the agent was not started again after it died"
        );
        hub.call(|c, now| {
            let fx = c
                .human_message(
                    &kd(),
                    "demo",
                    "after the crash",
                    &MessageOpts::default(),
                    now,
                )
                .map(|r| r.1)
                .unwrap_or_default();
            ((), fx)
        })
        .await;
        ensure!(
            eventually(async || std::fs::read_to_string(project.join("fake.log"))
                .is_ok_and(|s| s.contains("after the crash")))
            .await,
            "the restarted agent did not receive messages"
        );
        let _ = ipc::call(&dir, &Req::Shutdown).await;
        hub.shutdown().await;
        Ok("died once, started again, received a message".into())
    })
}

fn command_line() -> Probe {
    boxed(async {
        use clap::Parser;
        for args in [
            vec!["claudecord", "--detach"],
            vec!["claudecord", "login", "--hub", "https://hub.example.com"],
            vec!["claudecord", "logs", "otter"],
            vec!["claudecord", "handoff", "otter"],
            vec!["claudecord", "say", "hi"],
            vec!["claudecord", "done", "T1", "ok"],
            vec!["claudecord", "doctor"],
        ] {
            ensure!(
                crate::cli::Cli::try_parse_from(&args).is_ok(),
                "the command {args:?} does not parse"
            );
        }
        for args in [
            vec!["claudecord-hub", "hub"],
            vec!["claudecord-hub", "uptime", "--target", "99.9"],
            vec![
                "claudecord-hub",
                "probe",
                "https://hub.example.com",
                "--once",
            ],
            vec![
                "claudecord-hub",
                "load-tokens",
                "--count",
                "2",
                "--out",
                "t.json",
            ],
            vec!["claudecord-hub", "selftest"],
            vec!["claudecord-hub", "storage", "show"],
            vec!["claudecord-hub", "export", "--out", "v"],
        ] {
            ensure!(
                crate::cli::HubCli::try_parse_from(&args).is_ok(),
                "the command {args:?} does not parse"
            );
        }
        ensure!(
            crate::cli::Cli::try_parse_from(["claudecord", "nonsense"]).is_err(),
            "an unknown command was accepted"
        );
        // The two programs share no commands.
        for hub_only in [
            "serve",
            "storage",
            "token",
            "web-token",
            "uptime",
            "probe",
            "selftest",
        ] {
            ensure!(
                crate::cli::Cli::try_parse_from(["claudecord", hub_only]).is_err(),
                "claudecord accepted the hub command {hub_only}"
            );
        }
        Ok("every command parses, unknown commands refused".into())
    })
}

fn rollover_probe() -> Probe {
    boxed(async {
        let dir = scratch("rollover");
        let (db, seg) = (dir.join("hub.db"), dir.join("history"));
        let mut core = HubCore::default();
        core.add_owner("1");
        let cfg = ServerConfig {
            rollover_every: Some(Duration::from_millis(60)),
            hot_window: Duration::from_millis(1),
            ..server_cfg("127.0.0.1:0")
        };
        let hub = server::start(
            cfg,
            core,
            Store::open(&db, Some(&seg)).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
        for i in 0..3 {
            hub.call(move |c, now| {
                let fx = c
                    .human_message(
                        &kd(),
                        "p",
                        &format!("line {i}"),
                        &MessageOpts::default(),
                        now,
                    )
                    .map(|r| r.1)
                    .unwrap_or_default();
                ((), fx)
            })
            .await;
        }
        let rolled = eventually(async || {
            std::fs::read_dir(&seg).is_ok_and(|mut d| {
                d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz")))
            })
        })
        .await;
        ensure!(rolled, "old history was never moved into a compressed file");
        hub.shutdown().await;
        Ok(
            "old history moved out of the database into a compressed file by the hub's own timer"
                .into(),
        )
    })
}

/// A stand-in bucket service for the probe: keeps objects in memory and checks every request carries a signature.
async fn fake_bucket() -> (
    String,
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
) {
    use axum::{
        Router,
        body::Bytes,
        extract::{Path as P, State},
        http::{HeaderMap, Method, StatusCode},
        routing::any,
    };
    type Objects = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>;
    async fn handle(
        State(o): State<Objects>,
        m: Method,
        P(path): P<String>,
        h: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Vec<u8>) {
        if !h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 Credential="))
        {
            return (StatusCode::FORBIDDEN, vec![]);
        }
        let mut o = o.locked();
        match m {
            Method::PUT => {
                o.insert(path, body.to_vec());
                (StatusCode::OK, vec![])
            }
            Method::GET => o.get(&path).map_or((StatusCode::NOT_FOUND, vec![]), |b| {
                (StatusCode::OK, b.clone())
            }),
            _ => (StatusCode::NO_CONTENT, vec![]),
        }
    }
    let objects: Objects = Default::default();
    let app = Router::new()
        .route("/{*path}", any(handle))
        .with_state(objects.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (format!("http://{addr}"), objects)
}

fn bucket_probe() -> Probe {
    boxed(async {
        let (endpoint, objects) = fake_bucket().await;
        let dir = scratch("bucket");
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg")))
                .map_err(|e| e.to_string())?;
            s.set_bucket(Some(crate::store::bucket::Bucket {
                endpoint,
                region: "auto".into(),
                bucket: "h".into(),
                key_id: "k".into(),
                secret: "s".into(),
                prefix: String::new(),
            }));
            let row = |at: i64, t: &str| crate::store::HistoryRow {
                id: 0,
                at,
                project: "p".into(),
                thread: None,
                from: "kd".into(),
                kind: "human".into(),
                text: t.into(),
            };
            s.append(&[row(1, "old"), row(2, "older")])
                .map_err(|e| e.to_string())?;
            ensure!(
                s.rollover(500).map_err(|e| e.to_string())? == 2,
                "nothing was rolled over"
            );
            let seg = s.segments("p").map_err(|e| e.to_string())?.remove(0);
            ensure!(
                !dir.join("seg").join(&seg.file).exists(),
                "the file stayed on local disk"
            );
            ensure!(
                s.read_segment(&seg.file).map_err(|e| e.to_string())?.len() == 2,
                "history could not be read back from the bucket"
            );
            // The live database too: back it up, lose the disk, bring it back.
            s.backup_to_bucket(0).map_err(|e| e.to_string())?;
            let fresh = dir.join("fresh.db");
            Store::restore_from_bucket(s.bucket_ref().ok_or("no bucket")?, "backup/hub-latest.db.gz", &fresh, false).map_err(|e| e.to_string())?;
            ensure!(Store::open(&fresh, None).map_err(|e| e.to_string())?.projects().map_err(|e| e.to_string())? == vec!["p".to_string()], "the restored database lacks the history");
            Ok("rolled over, uploaded, removed locally, read back; database backed up and restored".to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        ensure!(
            objects.locked().keys().any(|k| k.ends_with(".jsonl.gz")),
            "the bucket does not hold the history file"
        );
        Ok(result)
    })
}

fn obsidian_probe() -> Probe {
    boxed(async {
        let rows = vec![
            crate::store::HistoryRow {
                id: 1,
                at: 1_369_398_600_000,
                project: "demo".into(),
                thread: Some("T1".into()),
                from: "otter".into(),
                kind: "say".into(),
                text: "@heron see T1".into(),
            },
            crate::store::HistoryRow {
                id: 2,
                at: 1_369_398_660_000,
                project: "demo".into(),
                thread: Some("T1".into()),
                from: "heron".into(),
                kind: "say".into(),
                text: "ok".into(),
            },
        ];
        let files: std::collections::HashMap<String, String> =
            crate::export::obsidian::render(&rows).into_iter().collect();
        let day = files
            .get("demo/Chat/T1/2013-05-24.md")
            .ok_or("no note for the day")?;
        ensure!(
            day.contains("[[People/heron|@heron]]") && day.contains("[[demo/Tasks/T1|T1]]"),
            "links to people and tasks are missing"
        );
        ensure!(
            files.contains_key("People/otter.md")
                && files.contains_key("demo/Tasks/T1.md")
                && files.contains_key("Home.md"),
            "people, task or home notes are missing"
        );
        let dir = scratch("obsidian");
        let n = crate::export::obsidian::write_vault(&dir, &crate::export::obsidian::render(&rows))
            .map_err(|e| e.to_string())?;
        ensure!(
            n > 0
                && crate::export::obsidian::write_vault(
                    &dir,
                    &crate::export::obsidian::render(&rows)
                )
                .map_err(|e| e.to_string())?
                    == 0,
            "writing twice was not a no-op the second time"
        );
        Ok(
            "notes for days, people, tasks and threads with links; second write changes nothing"
                .into(),
        )
    })
}

fn discord_probe() -> Probe {
    boxed(async {
        use crate::discord::{bridge, commands};
        let (fake, addr) = crate::discord::fake::start_fake().await;
        let dir = scratch("discord");
        let db = dir.join("hub.db");
        let mut store = Store::open(&db, None).map_err(|e| e.to_string())?;
        let token = store.create_token("mac", 0).map_err(|e| e.to_string())?;
        // This probe is not about heartbeats, so they are relaxed: a busy machine pausing for a fraction of a second must not get the device dropped (and
        // its agent forgotten) in the middle of the check.
        let slow = ServerConfig {
            ping_every: Duration::from_secs(2),
            ..server_cfg("127.0.0.1:0")
        };
        let hub = server::start(slow, HubCore::default(), store)
            .await
            .map_err(|e| e.to_string())?;
        let (tx, mut rx) = mpsc::channel(64);
        let l = link::spawn(
            format!("ws://{}/api/v1/node/connect", hub.addr),
            token,
            "mac".into(),
            tx,
            LinkOpts {
                ping_every: Duration::from_secs(2),
                ..link_opts()
            },
        );
        let cfg = bridge::BridgeConfig {
            scope: None,
            api_base: format!("http://{addr}/api"),
            token: "bot".into(),
            guild: "g1".into(),
            gateway_url: None,
            db_path: db,
            owners: vec![],
            backoff_max: Duration::from_millis(300),
        };
        bridge::spawn(hub.handle(), hub.chat(), cfg);
        ensure!(
            eventually(async || fake.log.locked().identifies >= 1).await,
            "the bridge never connected to the Discord gateway"
        );
        ensure!(
            fake.log.locked().commands.as_ref().is_some_and(|c| {
                c.as_array().is_some_and(|a| {
                    a.len() == commands::definitions().as_array().map_or(0, Vec::len)
                })
            }),
            "the slash commands were not registered"
        );
        l.send(crate::protocol::NodeFrame::AgentRegister {
            agent: crate::protocol::AgentSpec {
                agent_id: "demo/otter".into(),
                name: "otter".into(),
                project: "demo".into(),
                adapter: crate::protocol::AdapterId::Claude,
                model: None,
                role: None,
            },
            cwd: "/x".into(),
        })
        .await;
        ensure!(
            eventually(async || hub
                .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
                .await
                .unwrap_or(false))
            .await,
            "the agent never registered"
        );
        l.send(crate::protocol::NodeFrame::AgentSay {
            agent_id: "demo/otter".into(),
            text: "hello team".into(),
            thread: None,
            say_id: None,
        })
        .await;
        ensure!(
            eventually(async || fake
                .log
                .locked()
                .posts
                .iter()
                .any(|p| p["username"] == "otter" && p["content"] == "hello team"))
            .await,
            "an agent's words did not appear in Discord under its own name"
        );
        let channel = fake
            .log
            .locked()
            .channels
            .iter()
            .find(|c| c["name"] == "demo")
            .and_then(|c| c["id"].as_str().map(String::from))
            .ok_or("no project channel was made")?;
        let _ = fake.events.send(serde_json::json!({"op": 0, "s": 2, "t": "MESSAGE_CREATE", "d": {"id": "m1", "channel_id": channel, "content": "from discord", "author": {"id": "1", "username": "kd"}, "attachments": []}}).to_string());
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(e) = rx.recv().await {
                if matches!(e, LinkEvent::Frame(crate::protocol::HubFrame::Deliver { ref text, .. }) if text == "from discord") {
                    return true;
                }
            }
            false
        })
        .await;
        ensure!(
            got == Ok(true),
            "a message typed in Discord never reached the agent"
        );
        hub.shutdown().await;
        Ok("slash commands registered, agent posts under its own name, a Discord message reaches the agent".into())
    })
}

fn dashboard_probe() -> Probe {
    boxed(async {
        let dir = scratch("dashboard");
        let mut store = Store::open(&dir.join("hub.db"), None).map_err(|e| e.to_string())?;
        let web = store
            .create_token("web:probe", 0)
            .map_err(|e| e.to_string())?;
        let hub = server::start(server_cfg("127.0.0.1:0"), HubCore::default(), store)
            .await
            .map_err(|e| e.to_string())?;
        let get = |path: &'static str, token: Option<String>| {
            let url = format!("http://{}{path}", hub.addr);
            async move {
                let mut r = reqwest::Client::new().get(url);
                if let Some(t) = token {
                    r = r.header("authorization", format!("Bearer {t}"));
                }
                r.send()
                    .await
                    .map(|r| r.status().as_u16())
                    .map_err(|e| e.to_string())
            }
        };
        ensure!(
            get("/", None).await? == 200,
            "the dashboard page was not served"
        );
        ensure!(
            get("/api/v1/state", None).await? == 401,
            "the dashboard data was open without a token"
        );
        ensure!(
            get("/api/v1/state", Some(web)).await? == 200,
            "the dashboard data was refused to a dashboard token"
        );
        hub.shutdown().await;
        Ok("page served, data refused without a token and given to a dashboard token".into())
    })
}

/// The hub records itself, answers a prober, goes down on stop, and a debounced prober dates an outage from its first failure.
fn uptime_probe() -> Probe {
    boxed(async {
        let dir = scratch("uptime");
        let (hub, _, db) = hub(&dir, "127.0.0.1:0").await?;
        let base = format!("http://{}", hub.addr);
        ensure!(
            crate::uptime::probe_once(&base, Duration::from_secs(2)).await,
            "a prober did not see the hub as ready"
        );
        hub.shutdown().await;
        ensure!(
            !crate::uptime::probe_once(&base, Duration::from_millis(500)).await,
            "a prober still saw a stopped hub as up"
        );
        let log = Store::open(&db, None).map_err(|e| e.to_string())?;
        let changes = log.uptime_changes("hub", 0).map_err(|e| e.to_string())?;
        let states: Vec<_> = changes.iter().map(|c| c.state.as_str()).collect();
        ensure!(states == ["up", "down"], "the hub's record was {states:?}");
        let mut d = crate::uptime::Debounce::new(2);
        d.check(true, 0);
        d.check(false, 10);
        let outage = d.check(false, 20);
        ensure!(
            outage.is_some_and(|c| c.at == 10 && c.state == crate::uptime::State::Down),
            "the outage was not dated from the first failure"
        );
        Ok("start and stop recorded, ready check answered, outage dated from the first failed check".into())
    })
}

/// The whole sign-in against a tiny stand-in Discord: redirect, state check, code swap, a session that sees the project.
fn signin_probe() -> Probe {
    boxed(async {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        let fake = Router::new()
            .route(
                "/oauth2/token",
                post(|| async { Json(serde_json::json!({"access_token": "t"})) }),
            )
            .route(
                "/users/@me",
                get(|| async { Json(serde_json::json!({"id": "1", "username": "kd"})) }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let discord = format!("http://{}", l.local_addr().map_err(|e| e.to_string())?);
        tokio::spawn(async move {
            let _ = axum::serve(l, fake).await;
        });
        let dir = scratch("signin");
        let store = Store::open(&dir.join("hub.db"), None).map_err(|e| e.to_string())?;
        let mut core = HubCore::default();
        core.add_owner("1");
        let cfg = ServerConfig {
            oauth: Some(server::Oauth {
                client_id: "app".into(),
                client_secret: "s".into(),
                redirect_uri: "http://127.0.0.1/auth/callback".into(),
                authorize_url: format!("{discord}/authorize"),
                api_base: discord,
            }),
            ..server_cfg("127.0.0.1:0")
        };
        let hub = server::start(cfg, core, store)
            .await
            .map_err(|e| e.to_string())?;
        let base = format!("http://{}", hub.addr);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let start = http
            .get(format!("{base}/auth/login"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let cookie = |r: &reqwest::Response, n: &str| {
            r.headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(|c| {
                    c.strip_prefix(&format!("{n}="))
                        .map(|v| v.split(';').next().unwrap_or("").to_string())
                })
        };
        let state = cookie(&start, "cc_state").ok_or("no state cookie")?;
        let wrong = http
            .get(format!("{base}/auth/callback?code=x&state=bad"))
            .header("cookie", format!("cc_state={state}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        ensure!(wrong.status() == 400, "a wrong state was accepted");
        let back = http
            .get(format!("{base}/auth/callback?code=x&state={state}"))
            .header("cookie", format!("cc_state={state}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let session = cookie(&back, "cc_session").ok_or("signing in gave no session")?;
        let me = http
            .get(format!("{base}/auth/me"))
            .header("cookie", format!("cc_session={session}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        ensure!(me.status() == 200, "the session was not accepted");
        hub.shutdown().await;
        Ok("redirect, state check, code swap and a working session".into())
    })
}

/// A line is logged with its secret removed, a task that panics is started again, and a lock survives the panic of its holder.
fn robustness_probe() -> Probe {
    boxed(async {
        use crate::sync::Lock;
        let file = scratch("log").join("probe.log");
        crate::log::init(Some(file.clone()));
        let token = format!("ghp_{}", "q1W2".repeat(9));
        crate::info!("probe", "a line with a secret {token} in it");
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        ensure!(
            text.contains("probe: a line with a secret"),
            "the line was not logged: {text:?}"
        );
        ensure!(!text.contains(&token), "a secret reached the log");
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = runs.clone();
        let task = crate::task::supervised("probe task", move || {
            let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n < 2 {
                    panic!("on purpose");
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .map_err(|_| "the supervisor never finished")?
            .map_err(|e| e.to_string())?;
        ensure!(
            runs.load(std::sync::atomic::Ordering::SeqCst) == 3,
            "a task that panicked twice was not started again each time"
        );
        let lock = std::sync::Arc::new(std::sync::Mutex::new(1));
        let l2 = lock.clone();
        let _ = std::thread::spawn(move || {
            let _held = l2.lock().unwrap();
            panic!("while holding the lock");
        })
        .join();
        ensure!(
            *lock.locked() == 1,
            "the lock did not survive its holder's panic"
        );
        // What systemd would hear: a state line arrives at the socket it listens on.
        #[cfg(unix)]
        {
            let socket = scratch("notify").join("notify.sock");
            let listener =
                std::os::unix::net::UnixDatagram::bind(&socket).map_err(|e| e.to_string())?;
            listener
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|e| e.to_string())?;
            crate::notify::send_to(&socket.to_string_lossy(), "WATCHDOG=1");
            let mut buf = [0u8; 64];
            let n = listener
                .recv(&mut buf)
                .map_err(|e| format!("systemd was not told: {e}"))?;
            ensure!(&buf[..n] == b"WATCHDOG=1", "systemd heard something else");
        }
        Ok("logged without the secret, restarted a panicking task twice, used a lock after its holder panicked, told systemd it is alive".into())
    })
}
