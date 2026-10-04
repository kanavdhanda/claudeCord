//! The hub as a real program, started and stopped the way an operator does it: a normal stop (SIGTERM, which is what `systemctl stop`
//! and Docker send) saves everything and is recorded as a stop, while a kill is recorded as a crash from the last heartbeat. Unix only,
//! since it needs signals.
#![cfg(unix)]

use claudecord::store::Store;
use claudecord::uptime::State;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-robust-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Starts the real hub program on a free port and waits until it is ready.
fn start_hub(data: &std::path::Path) -> (Child, u16) {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    start_hub_on(data, port)
}

/// The same, on a port that is already chosen (to start the hub again where it was).
fn start_hub_on(data: &std::path::Path, port: u16) -> (Child, u16) {
    let already = std::fs::read_to_string(data.join("hub.log"))
        .unwrap_or_default()
        .matches("listening on")
        .count();
    let mut child = Command::new(env!("CARGO_BIN_EXE_claudecord"))
        .args(["hub", "--data"])
        .arg(data)
        .args(["--bind", &format!("127.0.0.1:{port}")])
        // Writes are slowed a little, so a kill is likely to land between a change being made and it being saved: that is the moment
        // the rule "nothing is acknowledged before it is on disk" has to hold.
        .env("CLAUDECORD_TEST_COMMIT_DELAY_MS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Ready is when the hub says it is listening in its log, which it does after it has recovered from the last run, installed its
    // stop handlers and opened its port, not merely when the port first accepts a connection.
    let up = Instant::now();
    while up.elapsed() < Duration::from_secs(20) {
        let log = std::fs::read_to_string(data.join("hub.log")).unwrap_or_default();
        if log.matches("listening on").count() > already {
            return (child, port);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Never leave the program running (or unreaped) when the test is about to fail.
    let _ = child.kill();
    let _ = child.wait();
    panic!("the hub never started");
}

fn signal(child: &Child, sig: &str) {
    assert!(
        Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let t = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the hub did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_normal_stop_by_sigterm_saves_everything_and_is_recorded_as_a_stop() {
    let data = dir("term");
    let (mut hub, _) = start_hub(&data);
    signal(&hub, "-TERM");
    let status = wait_exit(&mut hub);
    assert!(status.success(), "a stop is a success, got {status:?}");
    let log = std::fs::read_to_string(data.join("hub.log")).unwrap();
    assert!(
        log.contains("told to stop") && log.contains("stopped cleanly"),
        "{log}"
    );
    let db = Store::open(&data.join("hub.db"), None).unwrap();
    assert_eq!(
        db.uptime_last("hub").unwrap(),
        Some(State::Down),
        "recorded as stopped"
    );
    // Starting again finds nothing wrong.
    let (mut again, _) = start_hub(&data);
    signal(&again, "-TERM");
    wait_exit(&mut again);
    let log = std::fs::read_to_string(data.join("hub.log")).unwrap();
    assert!(
        !log.contains("did not stop cleanly"),
        "a clean stop is not reported as a crash: {log}"
    );
}

#[test]
fn a_kill_is_recorded_as_a_crash_from_the_last_heartbeat_and_the_next_start_says_so() {
    let data = dir("kill");
    let (mut hub, _) = start_hub(&data);
    signal(&hub, "-KILL");
    wait_exit(&mut hub);
    let (mut again, _) = start_hub(&data);
    let db = Store::open(&data.join("hub.db"), None).unwrap();
    let states: Vec<_> = db
        .uptime_changes("hub", 0)
        .unwrap()
        .iter()
        .map(|c| c.state.as_str())
        .collect();
    assert_eq!(
        states,
        ["up", "down", "up"],
        "up, killed, up again: the gap counts as downtime"
    );
    let log = std::fs::read_to_string(data.join("hub.log")).unwrap();
    assert!(log.contains("did not stop cleanly"), "{log}");
    signal(&again, "-TERM");
    wait_exit(&mut again);
}

/// Runs the hub program expecting it to refuse to start, and returns what it said.
fn refuses_to_start(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_claudecord"))
        .arg("hub")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "the hub started when it should not have"
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_hub_says_plainly_why_it_will_not_start_instead_of_failing_later() {
    // A damaged database.
    let data = dir("damaged");
    {
        let mut store = Store::open(&data.join("hub.db"), None).unwrap();
        let rows: Vec<_> = (0..2000)
            .map(|i| claudecord::store::HistoryRow {
                id: 0,
                at: i,
                project: "p".into(),
                thread: None,
                from: "w".into(),
                kind: "say".into(),
                text: format!("row {i} {}", "x".repeat(200)),
            })
            .collect();
        store.append(&rows).unwrap();
    }
    let mut bytes = std::fs::read(data.join("hub.db")).unwrap();
    for b in &mut bytes[8_192..12_288] {
        *b = 0xA5;
    }
    let _ = std::fs::remove_file(data.join("hub.db-wal"));
    let _ = std::fs::remove_file(data.join("hub.db-shm"));
    std::fs::write(data.join("hub.db"), &bytes).unwrap();
    let said = refuses_to_start(&["--data", data.to_str().unwrap(), "--bind", "127.0.0.1:0"]);
    assert!(
        said.contains("damaged") || said.contains("cannot be opened"),
        "{said}"
    );
    assert!(
        said.contains("storage restore") || said.contains("cannot be opened"),
        "it says what to do: {said}"
    );

    // A port already taken.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let data = dir("busy-port");
    let said = refuses_to_start(&[
        "--data",
        data.to_str().unwrap(),
        "--bind",
        &format!("127.0.0.1:{port}"),
    ]);
    assert!(said.contains("already in use"), "{said}");

    // A data folder nobody can write to (skipped when running as root, who can write anywhere).
    use std::os::unix::fs::PermissionsExt;
    let data = dir("readonly");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(data.join("probe"), b"x").is_err() {
        let said = refuses_to_start(&["--data", data.to_str().unwrap(), "--bind", "127.0.0.1:0"]);
        assert!(said.contains("cannot be written"), "{said}");
    }
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// The crash test. A machine sends numbered messages to the real hub program while the hub is killed (SIGKILL: no goodbye, no flush) at
// random moments and started again. What must hold: every message the hub acknowledged was already on disk when it was killed; and
// after the machine sends again what was never acknowledged, every message is saved exactly once, in order.

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// A machine connected to the hub, noting the highest number the hub has acknowledged.
struct Machine {
    tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    acked: Arc<AtomicU64>,
}

async fn connect_machine(port: u16, token: &str) -> Machine {
    let mut req = format!("ws://127.0.0.1:{port}/api/v1/node/connect")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut tx, mut rx) = ws.split();
    let acked = Arc::new(AtomicU64::new(0));
    let seen = acked.clone();
    tokio::spawn(async move {
        while let Some(Ok(m)) = rx.next().await {
            if let Message::Text(t) = m
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(t.as_str())
                && v["t"] == "ack"
                && let Some(n) = v["n"].as_u64()
            {
                seen.fetch_max(n, Ordering::SeqCst);
            }
        }
    });
    let reg = serde_json::json!({"t": "agent.register", "agent": {"agentId": "p/otter", "name": "otter", "project": "p", "adapter": "claude"}, "cwd": "/x"});
    tx.send(Message::Text(reg.to_string().into()))
        .await
        .unwrap();
    Machine { tx, acked }
}

fn numbered(epoch: u64, n: u64) -> Message {
    Message::Text(serde_json::json!({"t": "agent.say", "agentId": "p/otter", "text": format!("crash-{n:04}"), "e": epoch, "n": n}).to_string().into())
}

#[tokio::test]
async fn nothing_acknowledged_is_lost_when_the_hub_is_killed_at_random_moments_and_nothing_is_doubled()
 {
    const TOTAL: u64 = 600;
    const EPOCH: u64 = 99;
    let data = dir("crash");
    let tokens = data.join("tokens.json");
    assert!(
        Command::new(env!("CARGO_BIN_EXE_claudecord"))
            .args(["load-tokens", "--count", "1", "--prefix", "mac", "--out"])
            .arg(&tokens)
            .arg("--data")
            .arg(&data)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let token =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&tokens).unwrap())
            .unwrap()[0]["token"]
            .as_str()
            .unwrap()
            .to_string();
    let db = data.join("hub.db");

    let (mut hub, port) = start_hub(&data);
    let mut machine = connect_machine(port, &token).await;
    let mut sent_upto = 0u64;
    let mut rng = 12345u64;
    let mut crashes = 0;
    while machine.acked.load(Ordering::SeqCst) < TOTAL {
        // Send a burst of the next numbers, then kill the hub at a random moment inside it.
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let burst = 20 + (rng >> 40) % 60;
        let kill_after = (rng >> 20) % burst;
        for _ in 0..burst {
            if sent_upto >= TOTAL {
                break;
            }
            sent_upto += 1;
            if machine.tx.send(numbered(EPOCH, sent_upto)).await.is_err() {
                break;
            }
            if sent_upto % burst == kill_after && crashes < 6 && sent_upto < TOTAL {
                signal(&hub, "-KILL");
                wait_exit(&mut hub);
                crashes += 1;
                // At the instant of the kill: everything the hub had acknowledged is on disk.
                let acked = machine.acked.load(Ordering::SeqCst);
                let on_disk: std::collections::BTreeSet<String> = Store::open(&db, None)
                    .unwrap()
                    .history_latest("p", None, 5000)
                    .unwrap()
                    .into_iter()
                    .map(|r| r.text)
                    .collect();
                for n in 1..=acked {
                    assert!(
                        on_disk.contains(&format!("crash-{n:04}")),
                        "message {n} was acknowledged and then lost in a crash (crash {crashes})"
                    );
                }
                // The hub starts again; the machine reconnects and sends again what the hub never acknowledged, in order.
                let (h, _) = start_hub_on(&data, port);
                hub = h;
                machine = connect_machine(port, &token).await;
                for n in (acked + 1)..=sent_upto {
                    machine.tx.send(numbered(EPOCH, n)).await.unwrap();
                }
                break;
            }
        }
        // Give the hub a moment to catch up before the next burst.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    assert!(crashes >= 3, "the test killed the hub only {crashes} times");
    signal(&hub, "-TERM");
    wait_exit(&mut hub);
    // Every message, exactly once, in order.
    let rows = Store::open(&db, None).unwrap().history_all("p").unwrap();
    let texts: Vec<String> = rows
        .into_iter()
        .map(|r| r.text)
        .filter(|t| t.starts_with("crash-"))
        .collect();
    let want: Vec<String> = (1..=TOTAL).map(|n| format!("crash-{n:04}")).collect();
    assert_eq!(
        texts, want,
        "after {crashes} crashes, messages were lost, doubled or reordered"
    );
}

#[test]
fn the_hub_tells_systemd_when_it_is_ready_and_when_it_is_stopping() {
    use std::os::unix::net::UnixDatagram;
    // A socket standing in for systemd's notify socket, given to the hub through its environment (the child's only, not this process's).
    let data = dir("notify");
    let socket = data.join("notify.sock");
    let listener = UnixDatagram::bind(&socket).unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut hub = Command::new(env!("CARGO_BIN_EXE_claudecord"))
        .args(["hub", "--data"])
        .arg(&data)
        .args(["--bind", &format!("127.0.0.1:{port}")])
        .env("NOTIFY_SOCKET", &socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut buf = [0u8; 64];
    let n = listener
        .recv(&mut buf)
        .expect("systemd was never told the hub is ready");
    assert_eq!(&buf[..n], b"READY=1");
    signal(&hub, "-TERM");
    let n = listener
        .recv(&mut buf)
        .expect("systemd was never told the hub is stopping");
    assert_eq!(&buf[..n], b"STOPPING=1");
    wait_exit(&mut hub);
}

#[tokio::test]
async fn the_hub_keeps_an_obsidian_vault_up_to_date_while_it_runs_and_once_more_when_it_stops() {
    let data = dir("vault");
    let vault = data.join("vault");
    let tokens = data.join("tokens.json");
    assert!(
        Command::new(env!("CARGO_BIN_EXE_claudecord"))
            .args(["load-tokens", "--count", "1", "--prefix", "mac", "--out"])
            .arg(&tokens)
            .arg("--data")
            .arg(&data)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let token =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&tokens).unwrap())
            .unwrap()[0]["token"]
            .as_str()
            .unwrap()
            .to_string();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut hub = Command::new(env!("CARGO_BIN_EXE_claudecord"))
        .args(["hub", "--data"])
        .arg(&data)
        .args(["--bind", &format!("127.0.0.1:{port}"), "--vault"])
        .arg(&vault)
        .args(["--vault-every", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let up = Instant::now();
    while !std::fs::read_to_string(data.join("hub.log"))
        .unwrap_or_default()
        .contains("listening on")
    {
        assert!(
            up.elapsed() < Duration::from_secs(20),
            "the hub never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut machine = connect_machine(port, &token).await;
    for n in 1..=3 {
        machine.tx.send(numbered(1, n)).await.unwrap();
    }
    // Within a few seconds the vault has the conversation, while the hub is still running.
    let look = || -> String {
        let mut all = String::new();
        let mut stack = vec![vault.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "md") {
                    all.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        all
    };
    let t = Instant::now();
    while !look().contains("crash-0003") {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "the vault never got the messages while the hub ran"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // A message right before a stop is in the vault too, because the hub refreshes it once more after everything is saved.
    machine.tx.send(numbered(1, 4)).await.unwrap();
    while machine.acked.load(Ordering::SeqCst) < 4 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    signal(&hub, "-TERM");
    wait_exit(&mut hub);
    assert!(
        look().contains("crash-0004"),
        "the last message before the stop is missing from the vault"
    );
}
