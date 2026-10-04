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
    let mut child = Command::new(env!("CARGO_BIN_EXE_claudecord"))
        .args(["hub", "--data"])
        .arg(data)
        .args(["--bind", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let up = Instant::now();
    while up.elapsed() < Duration::from_secs(20) {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
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
