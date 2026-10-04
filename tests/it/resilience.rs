//! Behaviour on difficult machines: ones behind a web proxy, ones that were asleep, ones with no route to the hub. Also
//! the hub's list of which machines are active.

use claudecord::device::config::Config;
use claudecord::device::doctor;
use claudecord::device::link::{self, LinkEvent, LinkOpts, no_proxy_matches, resumed_from_sleep};
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
use claudecord::server::{self, Config as ServerConfig};
use claudecord::store::Store;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-res-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fast() -> LinkOpts {
    LinkOpts {
        ping_every: Duration::from_millis(100),
        // Generous: the first connection on a cold Windows runner can take more than a second.
        connect_timeout: Duration::from_secs(10),
        backoff_min: Duration::from_millis(30),
        backoff_max: Duration::from_millis(300),
        proxy: None,
    }
}

async fn hub_with_token(name: &str, node: &str) -> (server::Hub, String) {
    let dir = tmp(name);
    let mut store = Store::open(&dir.join("t.db"), None).unwrap();
    let token = store.create_token(node, 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let cfg = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ping_every: Duration::from_millis(100),
        tick_every: Duration::from_millis(50),
        ..ServerConfig::default()
    };
    (server::start(cfg, core, store).await.unwrap(), token)
}

/// A tiny web proxy that allows CONNECT. Records what it was asked for. With `refuse` it answers 407.
async fn proxy(refuse: bool) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = l.accept().await else {
                break;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let text = String::from_utf8_lossy(&head).to_string();
                log.lock().unwrap().push(text.clone());
                if refuse {
                    let _ = c
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                        .await;
                    return;
                }
                let target = text.split_whitespace().nth(1).unwrap().to_string();
                let Ok(mut up) = TcpStream::connect(&target).await else {
                    let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                let _ = c
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (addr, seen)
}

async fn expect_up(rx: &mut mpsc::Receiver<LinkEvent>, ms: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), async {
        while let Some(e) = rx.recv().await {
            if e == LinkEvent::Up {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn a_machine_that_must_use_a_web_proxy_connects_through_it_with_its_login() {
    let (hub, token) = hub_with_token("proxy", "mac").await;
    let (paddr, seen) = proxy(false).await;
    let (tx, mut rx) = mpsc::channel(16);
    let opts = LinkOpts {
        proxy: Some(format!("http://user:pw@{paddr}")),
        ..fast()
    };
    let _l = link::spawn(
        format!("ws://{}/api/v1/node/connect", hub.addr),
        token,
        "mac".into(),
        tx,
        opts,
    );
    assert!(
        expect_up(&mut rx, 3000).await,
        "connected through the proxy"
    );
    let first = seen.lock().unwrap()[0].clone();
    assert!(
        first.starts_with(&format!("CONNECT {} ", hub.addr)),
        "{first}"
    );
    assert!(
        first.contains("Proxy-Authorization: Basic dXNlcjpwdw=="),
        "the proxy login was sent: {first}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_proxy_that_refuses_is_reported_by_doctor_in_plain_words() {
    let (hub, token) = hub_with_token("proxy-no", "mac").await;
    let (paddr, _) = proxy(true).await;
    let cfg = Config {
        hub_url: format!("ws://{}", hub.addr),
        token,
        node_name: "mac".into(),
    };
    let checks = doctor::run(
        &cfg,
        &LinkOpts {
            proxy: Some(format!("http://{paddr}")),
            ..fast()
        },
    )
    .await;
    assert!(checks[0].ok && checks[0].detail.contains("through proxy"));
    let c = checks.iter().find(|c| c.name == "connect").unwrap();
    assert!(!c.ok && c.detail.contains("407"), "{c:?}");
    assert_eq!(checks.len(), 2, "later checks are skipped once one fails");
    hub.shutdown().await;
}

#[tokio::test]
async fn doctor_passes_end_to_end_and_names_the_failing_step_otherwise() {
    let (hub, token) = hub_with_token("doctor", "mac").await;
    let good = Config {
        hub_url: format!("ws://{}", hub.addr),
        token: token.clone(),
        node_name: "mac".into(),
    };
    let checks = doctor::run(&good, &fast()).await;
    assert_eq!(
        checks.iter().map(|c| (c.name, c.ok)).collect::<Vec<_>>(),
        vec![
            ("route", true),
            ("connect", true),
            ("welcome", true),
            ("heartbeat", true)
        ]
    );
    let bad_token = Config {
        token: "wrong".into(),
        ..good.clone()
    };
    let checks = doctor::run(&bad_token, &fast()).await;
    assert!(
        checks
            .iter()
            .any(|c| c.name == "connect" && !c.ok && c.detail.contains("401")),
        "{checks:?}"
    );
    let nowhere = Config {
        hub_url: "ws://127.0.0.1:1".into(),
        ..good
    };
    let checks = doctor::run(&nowhere, &fast()).await;
    assert!(
        checks
            .iter()
            .any(|c| c.name == "connect" && !c.ok && c.detail.contains("cannot reach")),
        "{checks:?}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn activity_cuts_a_long_pause_short_so_a_waking_machine_reconnects_at_once() {
    // A hub that will go down and come back on the same address, with the same database and so the same tokens.
    let dir = tmp("nudge");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    // Stand in for "no hub there": something listens on the address but hangs up on every connection, so the link's first attempt
    // fails at once on every platform (a closed port takes Windows about two seconds to refuse) and is counted, so the test knows
    // exactly when that attempt is over instead of guessing with a sleep.
    let door = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = door.local_addr().unwrap();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = attempts.clone();
    let hangup = tokio::spawn(async move {
        while let Ok((conn, _)) = door.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });
    let cfg = ServerConfig {
        bind,
        ping_every: Duration::from_millis(100),
        ..ServerConfig::default()
    };
    let (tx, mut rx) = mpsc::channel(16);
    // Pauses of 30 seconds: without a nudge the second attempt would be half a minute away.
    let slow = LinkOpts {
        backoff_min: Duration::from_secs(30),
        backoff_max: Duration::from_secs(30),
        ..fast()
    };
    let l = link::spawn(
        format!("ws://{bind}/api/v1/node/connect"),
        token,
        "mac".into(),
        tx,
        slow,
    );
    // The first attempt fails because no hub is there. Now the link is resting for about 30 seconds.
    while attempts.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    hangup.abort();
    let _ = hangup.await;
    let hub = server::start(cfg, HubCore::default(), Store::open(&db, None).unwrap())
        .await
        .unwrap();
    assert!(
        !expect_up(&mut rx, 600).await,
        "it is still resting, as it should be"
    );
    l.nudge();
    assert!(
        expect_up(&mut rx, 10_000).await,
        "a nudge (the machine is active) brings it back at once"
    );
    hub.shutdown().await;
}

#[test]
fn a_clock_jump_means_the_machine_was_asleep_and_a_normal_gap_does_not() {
    let t = SystemTime::now();
    assert!(!resumed_from_sleep(
        t,
        t + Duration::from_millis(1200),
        Duration::from_secs(1)
    ));
    assert!(
        !resumed_from_sleep(t, t + Duration::from_secs(10), Duration::from_secs(1)),
        "a busy machine can be late by seconds"
    );
    assert!(
        resumed_from_sleep(t, t + Duration::from_secs(600), Duration::from_secs(1)),
        "ten minutes is a suspend"
    );
    assert!(
        !resumed_from_sleep(t + Duration::from_secs(5), t, Duration::from_secs(1)),
        "a clock set backwards is not a wake-up"
    );
}

#[test]
fn no_proxy_lists_work_the_way_people_expect() {
    assert!(no_proxy_matches(
        "localhost,127.0.0.1,.corp.example",
        "localhost"
    ));
    assert!(no_proxy_matches(
        "localhost,127.0.0.1,.corp.example",
        "hub.corp.example"
    ));
    assert!(no_proxy_matches("*", "anything.example"));
    assert!(!no_proxy_matches("corp.example", "evilcorp.example"));
    assert!(!no_proxy_matches("", "hub.example"));
}

#[tokio::test]
async fn the_hub_lists_which_machines_are_active_and_when_they_were_last_heard() {
    let (hub, token) = hub_with_token("devices", "mac").await;
    let (tx, mut rx) = mpsc::channel(16);
    let l = link::spawn(
        format!("ws://{}/api/v1/node/connect", hub.addr),
        token,
        "mac".into(),
        tx,
        fast(),
    );
    assert!(expect_up(&mut rx, 2000).await);
    l.send(NodeFrame::AgentRegister {
        agent: AgentSpec {
            agent_id: "p/otter".into(),
            name: "otter".into(),
            project: "p".into(),
            adapter: AdapterId::Claude,
            model: None,
            role: None,
        },
        cwd: "/x".into(),
    })
    .await;
    let mut list = Vec::new();
    for _ in 0..100 {
        list = hub.devices().await;
        if list.first().is_some_and(|d| d.agents.len() == 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(list.len(), 1);
    assert!(list[0].connected && list[0].node == "mac" && list[0].agents == vec!["p/otter"]);
    let first_seen = list[0].last_seen.unwrap();
    // Heartbeat answers keep last_seen moving even when nothing else is said.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(
        hub.devices().await[0].last_seen.unwrap() > first_seen,
        "heartbeats count as signs of life"
    );
    drop(l);
    drop(rx);
    for _ in 0..100 {
        if !hub.devices().await[0].connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let d = &hub.devices().await[0];
    assert!(
        !d.connected && d.agents == vec!["p/otter"],
        "a machine that left stays listed, marked not connected"
    );
    hub.shutdown().await;
}
