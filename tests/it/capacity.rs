//! How much a hub carries from a person in chat to an agent's machine and back, with the stand-in Discord at one end and real device links at
//! the other. A measurement for the person running it; the checks that must hold in every CI run are in the other files.

use claudecord::device::link::{self, LinkEvent, LinkOpts};
use claudecord::discord::bridge::{self, BridgeConfig};
use claudecord::discord::fake::start_fake;
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use claudecord::server::{self, Config};
use claudecord::store::Store;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1)))
        .copied()
        .unwrap_or(f64::NAN)
}

/// The number after "msg" in a text.
fn marker(text: &str) -> Option<usize> {
    let rest = &text[text.find("msg")? + 3..];
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Run by hand (release build, so the numbers mean something):
///   CC_RATE=300 CC_SECS=10 CC_AGENTS=100 cargo test --release --test it person_to_agent -- --ignored --nocapture
/// People write to agents through the stand-in Discord at CC_RATE messages a second for CC_SECS seconds, spread over CC_AGENTS agents on
/// devices of ten agents each. Every agent answers each message at once. Prints how long a message took to reach its agent's device, and
/// to come back as a post in the channel, and how many were lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: prints latency and throughput (CC_RATE, CC_SECS, CC_AGENTS); needs a release build to mean anything"]
async fn person_to_agent_and_back() {
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (rate, secs, agents) = (
        env("CC_RATE", 100),
        env("CC_SECS", 10),
        env("CC_AGENTS", 50),
    );
    let total = rate * secs;
    let devices = agents.div_ceil(10);
    let (fake, addr) = start_fake().await;
    let dir = std::env::temp_dir().join(format!("cc-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("hub.db");
    let mut store = Store::open(&db, None).unwrap();
    let tokens: Vec<String> = (0..devices)
        .map(|d| store.create_token(&format!("d{d}"), 0).unwrap())
        .collect();
    let hub = server::start(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        },
        HubCore::default(),
        store,
    )
    .await
    .unwrap();
    bridge::spawn(
        hub.handle(),
        hub.chat(),
        BridgeConfig {
            scope: None,
            api_base: format!("http://{addr}/api"),
            token: "bottoken".into(),
            guild: "g1".into(),
            gateway_url: None,
            db_path: db.clone(),
            owners: vec![],
            backoff_max: Duration::from_millis(300),
        },
    );
    let delivered: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(vec![None; total]));
    let mut names = Vec::new();
    let mut links = Vec::new();
    for (d, token) in tokens.into_iter().enumerate() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let link = Arc::new(link::spawn(
            format!("ws://{}/api/v1/node/connect", hub.addr),
            token,
            format!("d{d}"),
            tx,
            LinkOpts {
                ping_every: Duration::from_secs(20),
                connect_timeout: Duration::from_secs(5),
                backoff_min: Duration::from_millis(50),
                backoff_max: Duration::from_millis(500),
                proxy: None,
            },
        ));
        let mine: Vec<String> = (0..10)
            .map(|j| d * 10 + j)
            .filter(|i| *i < agents)
            .map(|i| format!("a{i}"))
            .collect();
        names.extend(mine.clone());
        let (l, got) = (link.clone(), delivered.clone());
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                match e {
                    LinkEvent::Up => {
                        for n in &mine {
                            l.send(NodeFrame::AgentRegister {
                                agent: AgentSpec {
                                    agent_id: format!("demo/{n}"),
                                    name: n.clone(),
                                    project: "demo".into(),
                                    adapter: AdapterId::Claude,
                                    model: None,
                                    role: None,
                                },
                                cwd: "/x".into(),
                            })
                            .await;
                        }
                    }
                    LinkEvent::Frame(HubFrame::Deliver { agent_id, text, .. }) => {
                        if let Some(i) = marker(&text) {
                            if let Some(slot) = got.lock().unwrap().get_mut(i) {
                                slot.get_or_insert_with(Instant::now);
                            }
                            l.send(NodeFrame::AgentSay {
                                agent_id,
                                text: format!("re msg{i}"),
                                thread: None,
                            })
                            .await;
                        }
                    }
                    _ => {}
                }
            }
        });
        links.push(link);
    }
    let up = async || {
        let n = names.len();
        hub.call(move |c, _| (c.agents_of_project("demo").len() == n, vec![]))
            .await
            .unwrap_or(false)
    };
    for _ in 0..400 {
        if up().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up().await, "the agents did not all register");
    let channel = loop {
        let found = fake
            .log
            .lock()
            .unwrap()
            .channels
            .iter()
            .find(|c| c["name"] == "demo")
            .map(|c| c["id"].as_str().unwrap().to_string());
        if let Some(c) = found {
            break c;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    // Message ids must rise above what the bridge already took (it starts watching a channel from a current-looking id): these are far above.
    // Events sent before the bridge has signed in to the gateway are lost, as they would be in Discord.
    while fake.log.lock().unwrap().identifies < 1 {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let started = Instant::now();
    let (f, s, names2) = (fake.clone(), sent.clone(), names.clone());
    let sender = tokio::spawn(async move {
        for i in 0..total {
            tokio::time::sleep_until(
                (started + Duration::from_secs_f64(i as f64 / rate as f64)).into(),
            )
            .await;
            s.lock().unwrap().push(Instant::now());
            let msg = json!({"id": (2_000_000_000_000_000_000u64 + i as u64).to_string(), "channel_id": channel, "content": format!("@{} msg{i}", names2[i % names2.len()]), "author": {"id": "1", "username": "user1"}, "attachments": []});
            let _ = f
                .events
                .send(json!({"op": 0, "s": 2, "t": "MESSAGE_CREATE", "d": msg}).to_string());
        }
    });
    // Answers come back as webhook posts in the channel: note when each one first shows.
    let mut back: Vec<Option<Instant>> = vec![None; total];
    let (mut seen, mut count) = (0usize, 0usize);
    let deadline = started + Duration::from_secs(secs as u64 + 20);
    while count < total && Instant::now() < deadline {
        {
            let log = fake.log.lock().unwrap();
            for p in &log.posts[seen..] {
                if let Some(i) = p["content"]
                    .as_str()
                    .filter(|c| c.starts_with("re "))
                    .and_then(marker)
                    && back[i].is_none()
                {
                    back[i] = Some(Instant::now());
                    count += 1;
                }
            }
            seen = log.posts.len();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    sender.await.unwrap();
    let sent = sent.lock().unwrap().clone();
    let got = delivered.lock().unwrap().clone();
    let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
    let mut to_device: Vec<f64> = (0..sent.len())
        .filter_map(|i| got[i].map(|g| ms(sent[i], g)))
        .collect();
    let mut round: Vec<f64> = (0..sent.len())
        .filter_map(|i| back[i].map(|b| ms(sent[i], b)))
        .collect();
    println!(
        "{agents} agents on {devices} devices, {rate} msg/s for {secs} s: sent {}, reached a device {}, came back {}",
        sent.len(),
        to_device.len(),
        round.len()
    );
    for (what, v) in [
        ("person -> device", &mut to_device),
        ("person -> device -> chat", &mut round),
    ] {
        println!(
            "  {what}: p50 {:.0} ms, p95 {:.0} ms, p99 {:.0} ms",
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99)
        );
    }
    println!("  whole run {:.1} s", started.elapsed().as_secs_f64());
    drop(links);
    hub.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
