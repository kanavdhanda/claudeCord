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
        outbox: None,
    }
}

fn server_cfg(bind: &str) -> ServerConfig {
    ServerConfig {
        bind: bind.parse().unwrap(),
        ping_every: Duration::from_millis(100),
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
        harness: None,
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

#[tokio::test]
async fn frames_sent_while_the_hub_is_away_arrive_once_and_in_order_when_it_is_back() {
    use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
    let dir = std::env::temp_dir().join(format!("cc-link-away-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    // Pick an address, and start the machine's link before any hub is there.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = probe.local_addr().unwrap();
    drop(probe);
    let (tx, mut events) = mpsc::channel(256);
    let link = link::spawn(
        format!("ws://{bind}/api/v1/node/connect"),
        token,
        "mac".into(),
        tx,
        fast(),
    );
    link.send(NodeFrame::AgentRegister {
        agent: AgentSpec {
            agent_id: "p/otter".into(),
            name: "otter".into(),
            project: "p".into(),
            adapter: AdapterId::Claude,
            harness: None,
            model: None,
            role: None,
        },
        cwd: "/x".into(),
    })
    .await;
    for i in 0..30 {
        link.send(NodeFrame::AgentSay {
            agent_id: "p/otter".into(),
            text: format!("while away {i}"),
            thread: None,
            say_id: None,
        })
        .await;
    }
    // Only now does the hub start. The link was already waiting to retry, holding everything.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hub = server::start(
        ServerConfig {
            bind,
            ..server_cfg("127.0.0.1:0")
        },
        HubCore::default(),
        Store::open(&db, None).unwrap(),
    )
    .await
    .unwrap();
    let up = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = events.recv().await {
            if e == LinkEvent::Up {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(up, Ok(true));
    // Everything arrives, once each and in the order it was sent, with nothing resent or lost.
    let mut seen = Vec::new();
    for _ in 0..200 {
        seen = Store::open(&db, None)
            .unwrap()
            .history_latest("p", None, 500)
            .unwrap()
            .into_iter()
            .filter(|r| r.text.starts_with("while away"))
            .map(|r| r.text)
            .collect();
        if seen.len() >= 30 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let want: Vec<String> = (0..30).map(|i| format!("while away {i}")).collect();
    assert_eq!(seen, want, "frames were lost, doubled or reordered");
    hub.shutdown().await;
}

/// A hub that answers every request with `status` and counts the requests. Returns its address and the counter.
async fn status_server(
    status: &'static str,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (format!("http://{addr}"), hits)
}

#[tokio::test]
async fn a_hub_without_the_refresh_exchange_is_given_the_login_itself_and_only_asked_once() {
    use std::sync::atomic::Ordering;
    let (hub, hits) = status_server("404 Not Found").await;
    let auth = claudecord::device::link::Auth::new(&hub, "ccn1.login".into(), None);
    assert_eq!(auth.bearer().await.unwrap(), "ccn1.login");
    assert_eq!(auth.bearer().await.unwrap(), "ccn1.login");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "a hub that has no exchange is not asked again"
    );
}

#[tokio::test]
async fn a_refused_refresh_leaves_the_connection_to_say_so_and_an_unreachable_hub_is_an_error() {
    let (hub, _) = status_server("401 Unauthorized").await;
    let auth = claudecord::device::link::Auth::new(&hub, "ccn1.login".into(), None);
    // The login itself is tried next, and it is the hub's refusal of that which the person is told.
    assert_eq!(auth.bearer().await.unwrap(), "ccn1.login");
    let (hub, _) = status_server("500 Internal Server Error").await;
    let auth = claudecord::device::link::Auth::new(&hub, "ccn1.login".into(), None);
    assert!(auth.bearer().await.unwrap_err().contains("500"));
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let auth = claudecord::device::link::Auth::new(&closed, "ccn1.login".into(), None);
    assert!(auth.bearer().await.unwrap_err().contains("cannot reach"));
}

// ---- Restarts, exercised end to end: the real link against the real hub, the program and the hub stopped and started on the same files. ----

fn free_addr() -> std::net::SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = probe.local_addr().unwrap();
    drop(probe);
    a
}

/// The says in project `p`'s history, oldest first.
fn says(db: &std::path::Path, prefix: &str) -> Vec<String> {
    let v: Vec<String> = Store::open(db, None)
        .unwrap()
        .history_latest("p", None, 500)
        .unwrap()
        .into_iter()
        .filter(|r| r.text.starts_with(prefix))
        .map(|r| r.text)
        .collect();
    v
}

async fn wait_for_says(db: &std::path::Path, prefix: &str, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let v = says(db, prefix);
        if v.len() >= n {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    says(db, prefix)
}

async fn say_all(link: &link::Link, texts: &[&str]) {
    link.send(NodeFrame::AgentRegister {
        agent: spec("otter"),
        cwd: "/x".into(),
    })
    .await;
    for t in texts {
        link.send(NodeFrame::AgentSay {
            agent_id: "p/otter".into(),
            text: (*t).into(),
            thread: None,
            say_id: None,
        })
        .await;
    }
}

#[tokio::test]
async fn frames_the_hub_has_not_confirmed_survive_the_machine_program_restarting() {
    let dir = tmp("outbox-restart");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    let bind = free_addr();
    let url = format!("ws://{bind}/api/v1/node/connect");
    let outbox = dir.join("outbox.jsonl");
    let opts = || LinkOpts {
        outbox: Some(outbox.clone()),
        ..fast()
    };
    // The first run of the program says five things while no hub is reachable, then the program stops (as for an update or a crash).
    let (tx, ev) = mpsc::channel(256);
    let link = link::spawn(url.clone(), token.clone(), "mac".into(), tx, opts());
    say_all(&link, &["kept 1", "kept 2", "kept 3", "kept 4", "kept 5"]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(link);
    drop(ev);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let journal = std::fs::read_to_string(&outbox).unwrap();
    assert!(
        journal.matches("\"f\":").count() >= 6,
        "the unconfirmed frames are on disk: {journal}"
    );
    // The program starts again, says nothing new, and the hub comes up: all five arrive, once each and in order.
    let hub = start_hub(&bind.to_string(), &db).await;
    let (tx, mut ev) = mpsc::channel(256);
    let second = link::spawn(url.clone(), token.clone(), "mac".into(), tx, opts());
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    let want: Vec<String> = (1..=5).map(|i| format!("kept {i}")).collect();
    assert_eq!(wait_for_says(&db, "kept", 5).await, want);
    // And a third start finds nothing owed: still five, never doubled.
    drop(second);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (tx, mut ev) = mpsc::channel(256);
    let _third = link::spawn(url, token, "mac".into(), tx, opts());
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(says(&db, "kept"), want, "a frame was taken twice");
    hub.shutdown().await;
}

#[tokio::test]
async fn frames_the_hub_already_took_are_not_taken_again_when_their_acknowledgement_was_lost() {
    let dir = tmp("outbox-lost-ack");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    let bind = free_addr();
    let url = format!("ws://{bind}/api/v1/node/connect");
    let outbox = dir.join("outbox.jsonl");
    let opts = || LinkOpts {
        outbox: Some(outbox.clone()),
        ..fast()
    };
    let (tx, ev) = mpsc::channel(256);
    let link = link::spawn(url.clone(), token.clone(), "mac".into(), tx, opts());
    say_all(&link, &["once 1", "once 2", "once 3"]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // What the journal says before the hub has confirmed anything: this is what a program that stopped before hearing the acks would find.
    let before_acks = std::fs::read_to_string(&outbox).unwrap();
    drop(link);
    drop(ev);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let hub = start_hub(&bind.to_string(), &db).await;
    let (tx, mut ev) = mpsc::channel(256);
    let second = link::spawn(url.clone(), token.clone(), "mac".into(), tx, opts());
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    wait_for_says(&db, "once", 3).await;
    drop(second);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The acknowledgements are lost: the journal is as it was before them. The program starts, sends everything again under the same epoch,
    // and says one new thing. The hub takes only the new one.
    std::fs::write(&outbox, &before_acks).unwrap();
    let (tx, mut ev) = mpsc::channel(256);
    let third = link::spawn(url, token, "mac".into(), tx, opts());
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    third
        .send(NodeFrame::AgentSay {
            agent_id: "p/otter".into(),
            text: "once 4".into(),
            thread: None,
            say_id: None,
        })
        .await;
    let want: Vec<String> = (1..=4).map(|i| format!("once {i}")).collect();
    assert_eq!(wait_for_says(&db, "once", 4).await, want);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(says(&db, "once"), want, "a frame was taken twice");
    hub.shutdown().await;
}

#[tokio::test]
async fn after_the_hub_says_it_is_restarting_the_machine_keeps_trying_quickly_and_is_back_at_once()
{
    let dir = tmp("quick-back");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    drop(store);
    let bind = free_addr();
    let url = format!("ws://{bind}/api/v1/node/connect");
    // Pauses that would grow to 20 s if they were backing off, so a quick retry can only come from the restart rule.
    let opts = LinkOpts {
        backoff_min: Duration::from_millis(300),
        backoff_max: Duration::from_secs(20),
        ..fast()
    };
    let hub = start_hub(&bind.to_string(), &db).await;
    let (tx, mut ev) = mpsc::channel(256);
    let _link = link::spawn(url, token, "mac".into(), tx, opts);
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Up).await);
    // The hub stops (telling the machine it is restarting). For eight seconds something else answers on its address and counts the attempts.
    hub.shutdown().await;
    assert!(expect(&mut ev, 10_000, |e| *e == LinkEvent::Down).await);
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = attempts.clone();
    let counter = tokio::spawn(async move {
        loop {
            if let Ok((s, _)) = listener.accept().await {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(s);
            }
        }
    });
    tokio::time::sleep(Duration::from_secs(8)).await;
    counter.abort();
    let n = attempts.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        n >= 9,
        "tried {n} times in 8 s: backing off (about 5 or 6), not retrying every second or so"
    );
    // The hub is back: the machine is connected again within a moment, not after a long pause.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let hub = start_hub(&bind.to_string(), &db).await;
    assert!(
        expect(&mut ev, 1500, |e| *e == LinkEvent::Up).await,
        "slow to come back"
    );
    hub.shutdown().await;
}
