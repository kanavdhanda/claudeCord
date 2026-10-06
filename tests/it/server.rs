//! The hub over real sockets. These are the ways a connection can go wrong: a device that vanishes, one that stops
//! reading, a second connection for the same device, a hub that restarts, many devices at once, abuse. Each test starts
//! a real hub on a free port and talks to it with a real WebSocket client.

use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use claudecord::server::{self, Config};
use claudecord::store::Store;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    Error as WsError, Message, client::IntoClientRequest, protocol::CloseFrame,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async, connect_async};

type RawWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A device the way the real link behaves: a task of its own reads the socket all the time, so every ping is answered at once
/// whatever the test body is doing (sleeping, waiting on the hub, computing on a slow machine). A bare socket only answers pings
/// while the test happens to be reading it, so a healthy "device" would be dropped as silent whenever the test paused. Frames the
/// task reads are handed over through `next`.
struct Ws {
    tx: SplitSink<RawWs, Message>,
    rx: mpsc::UnboundedReceiver<Result<Message, WsError>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Ws {
    fn new(ws: RawWs) -> Self {
        let (tx, mut stream) = ws.split();
        let (to_test, rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            while let Some(m) = stream.next().await {
                if to_test.send(m).is_err() {
                    break;
                }
            }
        });
        Self { tx, rx, reader }
    }
    async fn send(&mut self, m: Message) -> Result<(), WsError> {
        self.tx.send(m).await
    }
    async fn next(&mut self) -> Option<Result<Message, WsError>> {
        self.rx.recv().await
    }
    async fn close(&mut self, why: Option<CloseFrame>) -> Result<(), WsError> {
        self.tx.send(Message::Close(why)).await?;
        self.tx.close().await
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A device that never reads, so it never answers a ping and its receive buffer fills: for the tests about devices that go
/// quiet. `buffer` shrinks the operating system's receive buffer, so the hub's backlog builds after kilobytes on every platform
/// (Windows would otherwise swallow megabytes first).
async fn connect_deaf(
    hub: &server::Hub,
    token: &str,
    buffer: Option<u32>,
) -> WebSocketStream<TcpStream> {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    if let Some(b) = buffer {
        socket.set_recv_buffer_size(b).unwrap();
    }
    let stream = socket.connect(hub.addr).await.unwrap();
    let mut req = format!("ws://{}/api/v1/node/connect", hub.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    client_async(req, stream).await.unwrap().0
}

fn kd() -> Human {
    Human {
        id: "1".into(),
        name: "kd".into(),
    }
}

fn cfg() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ping_every: Duration::from_millis(80),
        max_out_bytes: 1 << 20,
        tick_every: Duration::from_millis(50),
        ..Config::default()
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-srv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Starts a hub with owner kd, and a token for each of the named devices.
async fn boot(config: Config, db: &std::path::Path, nodes: &[&str]) -> (server::Hub, Vec<String>) {
    let mut store = Store::open(db, None).unwrap();
    let tokens = nodes
        .iter()
        .map(|n| store.create_token(n, 0).unwrap())
        .collect();
    let mut core = HubCore::default();
    core.add_owner("1");
    (server::start(config, core, store).await.unwrap(), tokens)
}

/// Starts a hub on an existing database, without making tokens.
async fn reboot(config: Config, db: &std::path::Path) -> server::Hub {
    server::start(config, HubCore::default(), Store::open(db, None).unwrap())
        .await
        .unwrap()
}

async fn connect(
    hub: &server::Hub,
    token: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{}/api/v1/node/connect", hub.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(ws, _)| Ws::new(ws))
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

async fn register(ws: &mut Ws, name: &str) {
    let f = NodeFrame::AgentRegister {
        agent: spec(name),
        cwd: "/x".into(),
    };
    ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
        .await
        .unwrap();
}

/// Next text frame from the hub, or None on close or timeout.
async fn next_frame(ws: &mut Ws, ms: u64) -> Option<HubFrame> {
    loop {
        match tokio::time::timeout(Duration::from_millis(ms), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return HubFrame::parse(t.as_str()),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}

async fn wait_for(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn connected(hub: &server::Hub, node: &'static str) -> bool {
    hub.call(move |c, _| (c.is_connected(node), vec![]))
        .await
        .unwrap_or(false)
}

#[tokio::test]
async fn a_bad_token_is_refused_and_a_good_one_is_welcomed() {
    let dir = tmp("auth");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    assert!(connect(&hub, "wrong").await.is_err());
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    assert!(
        matches!(next_frame(&mut ws, 1000).await, Some(HubFrame::Welcome { node_id }) if node_id == "mac")
    );
    hub.shutdown().await;
}

/// Like `connect`, as a proxy on this machine would pass it on: with the client's address in `X-Forwarded-For`.
async fn connect_via_proxy(
    hub: &server::Hub,
    token: &str,
    client: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{}/api/v1/node/connect", hub.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req.headers_mut()
        .insert("x-forwarded-for", client.parse().unwrap());
    connect_async(req).await.map(|(ws, _)| Ws::new(ws))
}

#[tokio::test]
async fn behind_a_proxy_only_the_misbehaving_address_is_blocked_not_everyone() {
    let dir = tmp("proxy");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    for _ in 0..10 {
        assert!(
            connect_via_proxy(&hub, "wrong", "203.0.113.9")
                .await
                .is_err()
        );
    }
    assert!(
        connect_via_proxy(&hub, &tokens[0], "203.0.113.9")
            .await
            .is_err(),
        "the one that failed is blocked"
    );
    assert!(
        connect_via_proxy(&hub, &tokens[0], "198.51.100.4")
            .await
            .is_ok(),
        "somebody else, through the same proxy, is not"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn repeated_bad_tokens_get_an_address_blocked() {
    let dir = tmp("block");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    for _ in 0..10 {
        assert!(connect(&hub, "wrong").await.is_err());
    }
    assert!(
        connect(&hub, &tokens[0]).await.is_err(),
        "even a good token is refused while blocked"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_message_reaches_the_device_and_acceptance_comes_back() {
    let dir = tmp("flow");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let mut chat = hub.chat();
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("agent registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    hub.call(|c, now| {
        let r = c
            .human_message(
                &kd(),
                "p",
                "hello",
                &MessageOpts {
                    reference: Some("c:1"),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        ((), r.1)
    })
    .await;
    let mut got = None;
    while let Some(f) = next_frame(&mut ws, 1000).await {
        if let HubFrame::Deliver { text, msg_id, .. } = f
            && text == "hello"
        {
            got = msg_id;
            break;
        }
    }
    let id = got.expect("the message arrived with an id");
    let f = NodeFrame::AgentAccepted {
        agent_id: "p/otter".into(),
        msg_ids: vec![id],
    };
    ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
        .await
        .unwrap();
    // The chat side is told to mark the original message as accepted.
    let confirmed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Chat::Confirm { reference, .. }) = chat.recv().await
                && reference == "c:1"
            {
                return true;
            }
        }
    })
    .await;
    assert_eq!(confirmed, Ok(true));
    hub.shutdown().await;
}

#[tokio::test]
async fn a_device_that_goes_silent_is_dropped() {
    let dir = tmp("silent");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let ws = connect_deaf(&hub, &tokens[0], None).await;
    wait_for("connected", async || connected(&hub, "mac").await).await;
    // Hold the socket open but never read it, so pings are never answered.
    let start = std::time::Instant::now();
    wait_for("dropped", async || !connected(&hub, "mac").await).await;
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "dropped within a few ping intervals, took {:?}",
        start.elapsed()
    );
    drop(ws);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_healthy_device_that_answers_pings_is_kept() {
    let dir = tmp("healthy");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let _ws = connect(&hub, &tokens[0]).await.unwrap();
    // Far longer than two ping intervals with nothing else going on: a device that answers pings is never dropped.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(connected(&hub, "mac").await);
    hub.shutdown().await;
}

// The rule: a device that stops reading is cut off once the hub has held more than its cap for it, so it can never make the hub
// hold an unbounded amount. Pings are turned off so only the backlog can end the connection, and the device's receive buffer is
// made tiny so the backlog starts after kilobytes whatever the operating system's own buffering would have been.
#[tokio::test]
async fn a_device_that_stops_reading_is_cut_off_instead_of_filling_the_hub() {
    let dir = tmp("slow");
    let (hub, tokens) = boot(
        Config {
            ping_every: Duration::from_secs(60),
            ..cfg()
        },
        &dir.join("t.db"),
        &["mac"],
    )
    .await;
    let mut ws = connect_deaf(&hub, &tokens[0], Some(4096)).await;
    let f = NodeFrame::AgentRegister {
        agent: spec("otter"),
        cwd: "/x".into(),
    };
    ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
        .await
        .unwrap();
    wait_for("agent registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // Never read again. Push well past the hub's cap for one device.
    for i in 0..8 {
        hub.call(move |c, _| {
            let (_, fx) = c
                .send_file(
                    &kd(),
                    "p",
                    "@otter",
                    "big.bin",
                    &vec![i as u8; 3 << 20],
                    None,
                    &format!("t{i}"),
                )
                .unwrap();
            ((), fx)
        })
        .await;
    }
    wait_for("dropped", async || !connected(&hub, "mac").await).await;
    drop(ws);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_second_connection_replaces_the_first() {
    let dir = tmp("replace");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let mut first = connect(&hub, &tokens[0]).await.unwrap();
    let _ = next_frame(&mut first, 500).await;
    let mut second = connect(&hub, &tokens[0]).await.unwrap();
    let mut told = false;
    while let Some(f) = next_frame(&mut first, 1000).await {
        if matches!(f, HubFrame::Error { message } if message.contains("replaced")) {
            told = true;
        }
    }
    assert!(told, "the first connection is told why");
    assert!(matches!(
        next_frame(&mut second, 500).await,
        Some(HubFrame::Welcome { .. })
    ));
    assert!(
        connected(&hub, "mac").await,
        "the first one closing does not evict the second"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn messages_wait_while_a_device_is_away_and_arrive_when_it_returns() {
    let dir = tmp("away");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    ws.close(None).await.unwrap();
    wait_for("gone", async || !connected(&hub, "mac").await).await;
    hub.call(|c, now| {
        let r = c
            .human_message(
                &kd(),
                "p",
                "while you were out",
                &MessageOpts::default(),
                now,
            )
            .unwrap();
        ((), r.1)
    })
    .await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    let mut got = false;
    while let Some(f) = next_frame(&mut ws, 1000).await {
        if matches!(f, HubFrame::Deliver { text, .. } if text == "while you were out") {
            got = true;
            break;
        }
    }
    assert!(got);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_restarted_hub_still_has_the_waiting_message_and_the_token() {
    let dir = tmp("restart");
    let db = dir.join("t.db");
    let (hub, tokens) = boot(cfg(), &db, &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    ws.close(None).await.unwrap();
    wait_for("gone", async || !connected(&hub, "mac").await).await;
    hub.call(|c, now| {
        let r = c
            .human_message(
                &kd(),
                "p",
                "survives a restart",
                &MessageOpts::default(),
                now,
            )
            .unwrap();
        ((), r.1)
    })
    .await;
    hub.shutdown().await;
    let hub = reboot(cfg(), &db).await;
    let mut ws = connect(&hub, &tokens[0])
        .await
        .expect("the token still works");
    register(&mut ws, "otter").await;
    let mut got = false;
    while let Some(f) = next_frame(&mut ws, 1000).await {
        if matches!(f, HubFrame::Deliver { text, .. } if text == "survives a restart") {
            got = true;
            break;
        }
    }
    assert!(got);
    hub.shutdown().await;
}

#[tokio::test]
async fn shutting_down_tells_devices_so_they_can_reconnect_at_once() {
    let dir = tmp("down");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    let _ = next_frame(&mut ws, 500).await;
    hub.shutdown().await;
    let mut code = None;
    while let Some(Ok(m)) = ws.next().await {
        if let Message::Close(Some(f)) = m {
            code = Some(u16::from(f.code));
            break;
        }
    }
    assert_eq!(code, Some(1001));
}

#[tokio::test]
async fn bad_frames_are_refused_without_hurting_the_connection_and_huge_ones_close_it() {
    let dir = tmp("abuse");
    let (hub, tokens) = boot(
        Config {
            max_frame: 64 * 1024,
            ..cfg()
        },
        &dir.join("t.db"),
        &["mac"],
    )
    .await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    let _ = next_frame(&mut ws, 500).await;
    ws.send(Message::Text("{\"t\":\"nonsense\"}".into()))
        .await
        .unwrap();
    assert!(
        matches!(next_frame(&mut ws, 1000).await, Some(HubFrame::Error { message }) if message == "bad frame")
    );
    register(&mut ws, "otter").await;
    wait_for("still works", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    ws.send(Message::Text("x".repeat(200_000).into()))
        .await
        .ok();
    wait_for("closed for size", async || !connected(&hub, "mac").await).await;
    hub.shutdown().await;
}

#[tokio::test]
async fn a_flood_of_frames_is_cut_off_by_the_rate_limit() {
    let dir = tmp("flood");
    let (hub, tokens) = boot(
        Config {
            ping_every: Duration::from_secs(60),
            ..cfg()
        },
        &dir.join("t.db"),
        &["mac"],
    )
    .await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    let hello = serde_json::to_string(&NodeFrame::Hello {
        node_name: "mac".into(),
        version: "t".into(),
        features: vec![],
    })
    .unwrap();
    for _ in 0..3000 {
        if ws.send(Message::Text(hello.clone().into())).await.is_err() {
            break;
        }
    }
    wait_for("dropped for flooding", async || {
        !connected(&hub, "mac").await
    })
    .await;
    hub.shutdown().await;
}

#[tokio::test]
async fn many_devices_can_connect_at_once_and_all_leave_cleanly() {
    let dir = tmp("many");
    let names: Vec<String> = (0..150).map(|i| format!("n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &refs).await;
    let mut sockets = Vec::new();
    for t in &tokens {
        sockets.push(connect(&hub, t).await.unwrap());
    }
    // The upgrade finishing and the hub's core hearing about it are two steps, so wait for the count to settle.
    wait_for("all 150 counted", async || {
        hub.call(|c, _| (c.connected_nodes(), vec![]))
            .await
            .unwrap()
            == 150
    })
    .await;
    for mut s in sockets {
        s.close(None).await.ok();
    }
    wait_for("all gone", async || {
        hub.call(|c, _| (c.connected_nodes(), vec![]))
            .await
            .unwrap()
            == 0
    })
    .await;
    hub.shutdown().await;
}

#[tokio::test]
async fn a_bug_in_one_handler_does_not_take_the_hub_down() {
    let dir = tmp("panic");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // Give the periodic save time to record the agent, then make a handler blow up.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut chat = hub.chat();
    let r: Option<()> = hub
        .call(|_, _| -> ((), Vec<Effect>) { panic!("simulated bug") })
        .await;
    assert!(
        r.is_none(),
        "the failed call reports failure instead of hanging"
    );
    // The failure is reported first: the project's chat is told, with the owner pinged, before anything else is said.
    let told = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(Chat::Notice {
                project,
                text,
                mention,
            }) = chat.recv().await
                && project == "p"
                && text.contains("Internal error")
            {
                return mention;
            }
        }
    })
    .await;
    assert_eq!(told, Ok(true), "the chat was told about the bug");
    // The hub is still alive, still knows the agent (restored from the save), and the device is still connected.
    assert!(connected(&hub, "mac").await, "the connection survived");
    wait_for("agent still known", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap_or(false)
    })
    .await;
    hub.call(|c, now| {
        let r = c
            .human_message(&kd(), "p", "still working?", &MessageOpts::default(), now)
            .unwrap();
        ((), r.1)
    })
    .await;
    let got = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(f) = next_frame(&mut ws, 1000).await {
            if matches!(f, HubFrame::Deliver { text, .. } if text == "still working?") {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(got, Ok(true), "messages still flow after the recovery");
    hub.shutdown().await;
}

#[tokio::test]
async fn old_history_is_moved_out_of_the_database_by_the_hub_on_its_own() {
    let dir = tmp("rollover");
    let db = dir.join("t.db");
    let seg = dir.join("history");
    let store = Store::open(&db, Some(&seg)).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(
        Config {
            rollover_every: Some(Duration::from_millis(60)),
            hot_window: Duration::from_millis(1),
            ..cfg()
        },
        core,
        store,
    )
    .await
    .unwrap();
    for i in 0..5 {
        hub.call(move |c, now| {
            let r = c
                .human_message(
                    &kd(),
                    "p",
                    &format!("line {i}"),
                    &MessageOpts::default(),
                    now,
                )
                .unwrap();
            ((), r.1)
        })
        .await;
    }
    wait_for("history rolled into a compressed file", async || {
        std::fs::read_dir(&seg).is_ok_and(|mut d| {
            d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz")))
        })
    })
    .await;
    let reader = Store::open(&db, Some(&seg)).unwrap();
    wait_for("the database kept none of it", async || {
        reader.hot_rows().unwrap() == 0
    })
    .await;
    let seg_info = reader.segments("p").unwrap();
    let back: usize = seg_info
        .iter()
        .map(|s| reader.read_segment(&s.file).unwrap().len())
        .sum();
    assert_eq!(
        back, 5,
        "all five lines are in the compressed files, none lost"
    );
    // The hub keeps working and keeps storing after a rollover.
    hub.call(|c, now| {
        let r = c
            .human_message(&kd(), "p", "after", &MessageOpts::default(), now)
            .unwrap();
        ((), r.1)
    })
    .await;
    hub.shutdown().await;
}

// The dashboard

async fn get(
    hub: &server::Hub,
    path: &str,
    token: Option<&str>,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut req = reqwest::Client::new().get(format!("http://{}{path}", hub.addr));
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let r = req.send().await.unwrap();
    (
        r.status().as_u16(),
        r.headers().clone(),
        r.text().await.unwrap(),
    )
}

#[tokio::test]
async fn the_dashboard_page_is_served_with_a_strict_policy_and_holds_no_data() {
    let dir = tmp("dash-page");
    // A machine name that cannot appear in ordinary page text, so finding it would mean the page holds data.
    let (hub, _) = boot(cfg(), &dir.join("t.db"), &["secret-box-7"]).await;
    let (code, headers, body) = get(&hub, "/", None).await;
    assert_eq!(code, 200);
    assert!(body.contains("claudeCord") && !body.contains("secret-box-7"));
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("script-src 'self'")
            && csp.contains("default-src 'none'")
            && csp.contains("frame-ancestors 'none'"),
        "{csp}"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let (code, _, js) = get(&hub, "/app.js", None).await;
    assert_eq!(code, 200);
    assert!(
        !js.contains("innerHTML") && !js.contains("eval("),
        "everything is drawn as text"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn only_a_dashboard_token_opens_the_data_and_it_never_connects_a_machine() {
    let dir = tmp("dash-auth");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let machine = store.create_token("mac", 0).unwrap();
    let web = store.create_token("web:kd", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(cfg(), core, store).await.unwrap();
    assert_eq!(get(&hub, "/api/v1/state", None).await.0, 401, "no token");
    assert_eq!(
        get(&hub, "/api/v1/state", Some(&machine)).await.0,
        401,
        "a machine's token does not open the dashboard"
    );
    let (code, headers, body) = get(&hub, "/api/v1/state", Some(&web)).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(
        connect(&hub, &web).await.is_err(),
        "a dashboard token does not connect a machine"
    );
    assert!(connect(&hub, &machine).await.is_ok());
    hub.shutdown().await;
}

#[tokio::test]
async fn the_dashboard_shows_machines_agents_waiting_questions_tasks_and_the_conversation() {
    let dir = tmp("dash-data");
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let machine = store.create_token("mac", 0).unwrap();
    let web = store.create_token("web:kd", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let hub = server::start(cfg(), core, store).await.unwrap();
    let mut ws = connect(&hub, &machine).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let f = NodeFrame::AgentAsk {
        agent_id: "p/otter".into(),
        ask_id: "a1".into(),
        question: "which db?".into(),
        options: None,
        thread: None,
    };
    ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
        .await
        .unwrap();
    hub.call(|c, now| {
        let r = c
            .human_message(
                &kd(),
                "p",
                "<script>alert(1)</script> hello",
                &MessageOpts::default(),
                now,
            )
            .unwrap();
        ((), r.1)
    })
    .await;
    wait_for("the question shows", async || {
        get(&hub, "/api/v1/state", Some(&web))
            .await
            .2
            .contains("which db?")
    })
    .await;
    let (_, _, body) = get(&hub, "/api/v1/state", Some(&web)).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["devices"][0]["node"], "mac");
    assert_eq!(v["devices"][0]["connected"], true);
    assert_eq!(v["projects"]["p"]["agents"][0]["name"], "otter");
    assert_eq!(v["projects"]["p"]["agents"][0]["lead"], true);
    assert_eq!(v["projects"]["p"]["asks"][0]["id"], "Q1");
    wait_for("history rows visible", async || {
        get(&hub, "/api/v1/history?project=p&latest=1", Some(&web))
            .await
            .2
            .contains("hello")
    })
    .await;
    let (code, _, hist) = get(
        &hub,
        "/api/v1/history?project=p&latest=1&limit=10",
        Some(&web),
    )
    .await;
    assert_eq!(code, 200);
    assert!(
        hist.contains("alert(1)"),
        "the text is sent as it is; the page draws it as text, never as HTML"
    );
    assert_eq!(
        get(&hub, "/api/v1/history", Some(&web)).await.0,
        400,
        "a project is required"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn one_misbehaving_device_never_disturbs_another() {
    let dir = tmp("badneighbour");
    let (hub, tokens) = boot(cfg(), &dir.join("t.db"), &["good", "bad"]).await;
    let mut good = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut good, "otter").await;
    wait_for("the good device's agent", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // The other device does everything wrong at once: binary junk, text that is not JSON, JSON of the wrong shape, an oversized
    // frame, then it vanishes without a goodbye.
    let mut bad = connect_deaf(&hub, &tokens[1], None).await;
    for junk in [
        Message::Binary(vec![0xff, 0x00, 0xfe, 0x01].into()),
        Message::Text("{{{{ not json".into()),
        Message::Text("{\"t\":\"agent.say\"}".into()),
        Message::Text("\u{0}\u{1b}[31m".into()),
        Message::Text("x".repeat(300_000).into()),
    ] {
        let _ = bad.send(junk).await;
    }
    drop(bad);
    // Whatever the bad one did, the hub is up and the good device still gets its mail.
    hub.call(|c, now| {
        let fx = c
            .human_message(&kd(), "p", "still here?", &MessageOpts::default(), now)
            .unwrap()
            .1;
        ((), fx)
    })
    .await;
    let mut got = false;
    for _ in 0..40 {
        if let Some(HubFrame::Deliver { text, .. }) = next_frame(&mut good, 250).await
            && text.contains("still here?")
        {
            got = true;
            break;
        }
    }
    assert!(got, "the good device kept receiving");
    assert!(connected(&hub, "good").await);
    wait_for("the bad device is gone", async || {
        !connected(&hub, "bad").await
    })
    .await;
    hub.shutdown().await;
}

#[tokio::test]
async fn past_its_limit_the_hub_refuses_new_devices_with_try_later_and_takes_them_again_when_room_returns()
 {
    let dir = tmp("cap");
    let (hub, tokens) = boot(
        Config {
            max_devices: 2,
            ..cfg()
        },
        &dir.join("t.db"),
        &["a", "b", "c"],
    )
    .await;
    let _a = connect(&hub, &tokens[0]).await.unwrap();
    let mut b = connect(&hub, &tokens[1]).await.unwrap();
    let third = connect(&hub, &tokens[2]).await;
    assert!(
        matches!(&third, Err(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 503),
        "the third device is told to try later: {:?}",
        third.as_ref().err()
    );
    // The two that are in are not disturbed, and a place given back is taken again.
    b.close(None).await.unwrap();
    wait_for("room again", async || !connected(&hub, "b").await).await;
    let again = connect(&hub, &tokens[2]).await;
    assert!(again.is_ok(), "{:?}", again.err());
    hub.shutdown().await;
}

/// A numbered `agent.say`, the way the link sends it.
fn say(epoch: u64, n: u64, text: &str) -> Message {
    Message::Text(
        serde_json::json!({"t": "agent.say", "agentId": "p/otter", "text": text, "e": epoch, "n": n})
            .to_string()
            .into(),
    )
}

/// The next ack the hub sends, or None.
async fn next_ack(ws: &mut Ws, ms: u64) -> Option<u64> {
    loop {
        match next_frame(ws, ms).await? {
            HubFrame::Ack { n } => return Some(n),
            _ => continue,
        }
    }
}

/// How many times `text` appears in a project's saved history.
fn saved_times(db: &std::path::Path, text: &str) -> usize {
    Store::open(db, None)
        .unwrap()
        .history_latest("p", None, 500)
        .unwrap()
        .iter()
        .filter(|r| r.text.contains(text))
        .count()
}

#[tokio::test]
async fn an_ack_comes_only_when_the_change_is_on_disk_and_a_frame_sent_twice_is_taken_once() {
    let dir = tmp("acks");
    let db = dir.join("t.db");
    let (hub, tokens) = boot(cfg(), &db, &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    // Each frame's ack means it is already saved.
    for n in 1..=20u64 {
        ws.send(say(7, n, &format!("unique line {n}")))
            .await
            .unwrap();
        assert_eq!(
            next_ack(&mut ws, 3000).await,
            Some(n),
            "frame {n} was never acknowledged"
        );
        assert_eq!(
            saved_times(&db, &format!("unique line {n}")),
            1,
            "frame {n} was acknowledged before it was on disk"
        );
    }
    // The same number again (a machine that did not see the ack and sends it once more): not taken twice, but acknowledged again.
    ws.send(say(7, 20, "unique line 20")).await.unwrap();
    assert_eq!(next_ack(&mut ws, 3000).await, Some(20));
    assert_eq!(
        saved_times(&db, "unique line 20"),
        1,
        "a repeated frame was taken twice"
    );
    // A new epoch is a machine that restarted: its number 1 is new again.
    ws.send(say(8, 1, "after a restart")).await.unwrap();
    assert_eq!(next_ack(&mut ws, 3000).await, Some(1));
    assert_eq!(saved_times(&db, "after a restart"), 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_frame_resent_after_the_hub_restarted_is_not_taken_twice() {
    let dir = tmp("acks-restart");
    let db = dir.join("t.db");
    let (hub, tokens) = boot(cfg(), &db, &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    ws.send(say(5, 1, "before the restart")).await.unwrap();
    assert_eq!(next_ack(&mut ws, 3000).await, Some(1));
    hub.shutdown().await;
    // The hub comes back from its saved state; the machine, not sure the ack arrived, sends the same frame again.
    let hub = reboot(cfg(), &db).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    ws.send(say(5, 1, "before the restart")).await.unwrap();
    assert_eq!(next_ack(&mut ws, 3000).await, Some(1), "acknowledged again");
    hub.shutdown().await;
    assert_eq!(
        saved_times(&db, "before the restart"),
        1,
        "the restart forgot what it had taken"
    );
}

#[tokio::test]
async fn the_dashboard_route_pages_back_into_the_compressed_files() {
    let dir = tmp("pages");
    let db = dir.join("t.db");
    let seg = dir.join("history");
    {
        let mut s = Store::open(&db, Some(&seg)).unwrap();
        let rows: Vec<_> = (0..300)
            .map(|i| claudecord::store::HistoryRow {
                id: 0,
                at: (i / 100) * 86_400_000 + i,
                project: "p".into(),
                thread: None,
                from: "kd".into(),
                kind: "say".into(),
                text: format!("line {i:03}"),
            })
            .collect();
        s.append(&rows).unwrap();
        s.rollover(2 * 86_400_000).unwrap();
    }
    let mut store = Store::open(&db, Some(&seg)).unwrap();
    let token = store.create_token("web:viewer", 0).unwrap();
    let hub = server::start(cfg(), HubCore::default(), store)
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let get = |q: String| {
        let (http, base, token) = (http.clone(), format!("http://{}", hub.addr), token.clone());
        async move {
            http.get(format!("{base}/api/v1/history?project=p&{q}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json::<Vec<serde_json::Value>>()
                .await
                .unwrap()
        }
    };
    // The newest page is from the database; the page before it starts in the compressed files.
    let newest = get("latest=1&limit=50".into()).await;
    assert_eq!(newest.last().unwrap()["text"], "line 299");
    let mut oldest = newest[0]["id"].as_i64().unwrap();
    let mut seen = newest.len();
    loop {
        let page = get(format!("before={oldest}&limit=50")).await;
        if page.is_empty() {
            break;
        }
        assert!(
            page.iter().all(|r| r["id"].as_i64().unwrap() < oldest),
            "a page holds only older rows"
        );
        oldest = page[0]["id"].as_i64().unwrap();
        seen += page.len();
    }
    assert_eq!(seen, 300, "paging back reached every row exactly once");
    hub.shutdown().await;
}

/// A device that is connected and quiet must cost the hub a few kilobytes, not the 128 KB read buffer and 128 KB write buffer a WebSocket
/// gets by default: at 100 KB each, 5,000 machines were 500 MB before they said anything.
// Memory is read with `ps`, which Windows does not have.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_device_costs_the_hub_kilobytes_not_hundreds() {
    use tokio_tungstenite::connect_async_with_config;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    const N: usize = 400;
    let names: Vec<String> = (0..N).map(|i| format!("m{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let dir = tmp("quiet");
    let (hub, tokens) = boot(
        Config {
            ping_every: Duration::from_secs(30),
            ..cfg()
        },
        &dir.join("t.db"),
        &refs,
    )
    .await;
    // The test's own end of each link is as small as it can be, so what is measured is the hub's.
    let small = WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(0);
    let mut links = Vec::new();
    let before = super::procs::rss_kb();
    for t in &tokens {
        let mut req = format!("ws://{}/api/v1/node/connect", hub.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
        let (ws, _) = connect_async_with_config(req, Some(small), false)
            .await
            .unwrap();
        links.push(Ws::new(ws));
    }
    wait_for("all connected", async || {
        hub.devices().await.iter().filter(|d| d.connected).count() == N
    })
    .await;
    let per_device = (super::procs::rss_kb().saturating_sub(before)) / N;
    assert!(
        per_device < 48,
        "{per_device} KB of memory per quiet device (this program, hub and test ends together)"
    );
    drop(links);
    hub.shutdown().await;
}

/// The disk fails for a while (here: the history table is out of reach, which makes every write fail the same way a full or read-only disk
/// does) and then comes back. The hub keeps serving, as it says it does; what it took in meanwhile must not be gone for good once the disk
/// is back, because the device was told it was saved and will never send it again.
#[tokio::test]
async fn what_arrived_while_the_disk_was_failing_is_written_when_it_recovers() {
    let dir = tmp("diskfail");
    let db = dir.join("t.db");
    let (hub, tokens) = boot(cfg(), &db, &["mac"]).await;
    let mut ws = connect(&hub, &tokens[0]).await.unwrap();
    register(&mut ws, "otter").await;
    wait_for("registered", async || {
        hub.call(|c, _| (c.agent("p/otter").is_some(), vec![]))
            .await
            .unwrap()
    })
    .await;
    let other = rusqlite::Connection::open(&db).unwrap();
    other.busy_timeout(Duration::from_secs(5)).unwrap();
    other
        .execute_batch("ALTER TABLE history RENAME TO history_away")
        .unwrap();
    for n in 1..=3u64 {
        ws.send(say(7, n, &format!("while the disk failed {n}")))
            .await
            .unwrap();
    }
    // The hub carries on (it answers) although every write fails.
    assert!(connected(&hub, "mac").await, "the hub must keep serving");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    other
        .execute_batch("ALTER TABLE history_away RENAME TO history")
        .unwrap();
    ws.send(say(7, 4, "after the disk came back"))
        .await
        .unwrap();
    wait_for("the later message saved", async || {
        saved_times(&db, "after the disk came back") == 1
    })
    .await;
    for n in 1..=3 {
        assert_eq!(
            saved_times(&db, &format!("while the disk failed {n}")),
            1,
            "message {n}, which the hub took in while the disk was failing, never reached the disk"
        );
    }
    hub.shutdown().await;
}
