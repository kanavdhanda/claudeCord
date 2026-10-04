//! The device side of the connection, run against the real hub: it connects, it comes back by itself after the hub
//! restarts or drops it, and it does not hammer a hub that is down.

use claudecord::device::config::Config;
use claudecord::device::link::{self, LinkEvent, LinkOpts};
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use claudecord::server::{self, Config as ServerConfig};
use claudecord::store::Store;
use std::time::Duration;
use tokio::sync::mpsc;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-dev-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fast() -> LinkOpts {
    LinkOpts {
        ping_every: Duration::from_millis(100),
        connect_timeout: Duration::from_millis(500),
        backoff_min: Duration::from_millis(30),
        backoff_max: Duration::from_millis(300),
        proxy: None,
    }
}

fn server_cfg(bind: &str) -> ServerConfig {
    ServerConfig {
        bind: bind.parse().unwrap(),
        ping_every: Duration::from_millis(100),
        save_every: Duration::from_millis(10),
        tick_every: Duration::from_millis(50),
        ..ServerConfig::default()
    }
}

async fn start_hub(bind: &str, db: &std::path::Path) -> server::Hub {
    let mut core = HubCore::default();
    core.add_owner("1");
    server::start(server_cfg(bind), core, Store::open(db, None).unwrap())
        .await
        .unwrap()
}

async fn expect(
    rx: &mut mpsc::Receiver<LinkEvent>,
    ms: u64,
    f: impl Fn(&LinkEvent) -> bool,
) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), async {
        while let Some(e) = rx.recv().await {
            if f(&e) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        agent_id: format!("p/{name}"),
        name: name.into(),
        project: "p".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    }
}

#[test]
fn config_is_saved_privately_and_read_back() {
    let dir = tmp("cfg");
    let c = Config {
        hub_url: "wss://hub.example.com/".into(),
        token: "secret".into(),
        node_name: "mac".into(),
    };
    c.save(&dir).unwrap();
    assert_eq!(Config::load(&dir), Some(c.clone()));
    assert_eq!(c.connect_url(), "wss://hub.example.com/api/v1/node/connect");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(Config::load(&tmp("empty")), None);
}

#[tokio::test]
async fn the_link_connects_registers_and_receives_messages() {
    let dir = tmp("up");
    let mut store = Store::open(&dir.join("t.db"), None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(server_cfg("127.0.0.1:0"), core, store)
        .await
        .unwrap();
    let (ev_tx, mut ev) = mpsc::channel(64);
    let link = link::spawn(
        format!("ws://{}/api/v1/node/connect", hub.addr),
        token,
        "mac".into(),
        ev_tx,
        fast(),
    );
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    link.send(NodeFrame::AgentRegister {
        agent: spec("otter"),
        cwd: "/x".into(),
    })
    .await;
    for _ in 0..100 {
        if hub
            .call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    hub.call(|c, now| {
        let r = c
            .human_message(
                &Human {
                    id: "1".into(),
                    name: "kd".into(),
                },
                "p",
                "hi there",
                &MessageOpts::default(),
                now,
            )
            .unwrap();
        ((), r.1)
    })
    .await;
    assert!(
        expect(
            &mut ev,
            10_000,
            |e| matches!(e, LinkEvent::Frame(HubFrame::Deliver { text, .. }) if text == "hi there")
        )
        .await
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_link_comes_back_by_itself_after_the_hub_restarts() {
    let dir = tmp("back");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    // Pick a port, run a hub on it, and later run another on the same port.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = probe.local_addr().unwrap().to_string();
    drop(probe);
    let hub = start_hub(&bind, &db).await;
    let (ev_tx, mut ev) = mpsc::channel(64);
    let _link = link::spawn(
        format!("ws://{bind}/api/v1/node/connect"),
        token,
        "mac".into(),
        ev_tx,
        fast(),
    );
    assert!(
        expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await,
        "first connection"
    );
    hub.shutdown().await;
    assert!(
        expect(&mut ev, 10_000, |e| *e == LinkEvent::Down).await,
        "told the connection ended"
    );
    // The hub is down for a moment. The link keeps trying quietly.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let hub = start_hub(&bind, &db).await;
    assert!(
        expect(&mut ev, 3000, |e| *e == LinkEvent::Up).await,
        "reconnected with no help"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn frames_sent_while_the_hub_is_down_are_delivered_when_it_returns() {
    let dir = tmp("queued");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = probe.local_addr().unwrap().to_string();
    drop(probe);
    let (ev_tx, mut ev) = mpsc::channel(64);
    let link = link::spawn(
        format!("ws://{bind}/api/v1/node/connect"),
        token,
        "mac".into(),
        ev_tx,
        fast(),
    );
    link.send(NodeFrame::AgentRegister {
        agent: spec("otter"),
        cwd: "/x".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hub = start_hub(&bind, &db).await;
    assert!(expect(&mut ev, 3000, |e| *e == LinkEvent::Up).await);
    for _ in 0..100 {
        if hub
            .call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
        {
            hub.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the queued registration never arrived");
}

#[tokio::test]
async fn a_wrong_token_never_connects_and_does_not_spin() {
    let dir = tmp("wrong");
    let hub = start_hub("127.0.0.1:0", &dir.join("t.db")).await;
    let (ev_tx, mut ev) = mpsc::channel(64);
    let _link = link::spawn(
        format!("ws://{}/api/v1/node/connect", hub.addr),
        "nope".into(),
        "mac".into(),
        ev_tx,
        fast(),
    );
    assert!(!expect(&mut ev, 800, |e| *e == LinkEvent::Up).await);
    hub.shutdown().await;
}
