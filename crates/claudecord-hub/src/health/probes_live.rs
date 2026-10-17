//! Health probes for the running pieces: the hub server, Discord bridge, dashboard, and commands.

use super::{Feature, Probe, boxed, eventually, scratch};
use crate::hub::*;
use crate::server::{self, Config as ServerConfig};
use crate::store::Store;
use crate::sync::Lock;
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

macro_rules! ensure {
    ($c:expr, $($m:tt)*) => {
        if !$c {
            return Err(format!($($m)*));
        }
    };
}

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

async fn connect_ws(
    url: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
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
            name: "command line",
            covers: &["cli"],
            probe: command_line,
        },
    ]
}

fn server_probe() -> Probe {
    boxed(async {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let dir = scratch("server");
        let (hub, token, db) = hub(&dir, "127.0.0.1:0").await?;
        let url = format!("ws://{}/api/v1/node/connect", hub.addr);
        let bad = connect_ws(&url, "wrong").await;
        ensure!(
            matches!(&bad, Err(e) if e.contains("401")),
            "a wrong token was not refused"
        );
        let ws = connect_ws(&url, &token)
            .await
            .map_err(|e| format!("could not connect: {e}"))?;
        let (mut ws_tx, mut ws_rx) = ws.split();
        let up = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(msg) = ws_rx.next().await {
                if let Ok(Message::Text(t)) = msg
                    && let Ok(crate::protocol::HubFrame::Welcome { .. }) = serde_json::from_str(&t)
                {
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
        let _reader = tokio::spawn(async move { while let Some(Ok(_)) = ws_rx.next().await {} });
        let reg = serde_json::to_string(&crate::protocol::NodeFrame::AgentRegister {
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
        .unwrap();
        ws_tx
            .send(Message::Text(reg.into()))
            .await
            .map_err(|e| e.to_string())?;
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
        let stop_ping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sp = stop_ping.clone();
        let ping_loop = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(80));
            while !sp.load(std::sync::atomic::Ordering::Relaxed) {
                interval.tick().await;
                if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(350)).await;
        stop_ping.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = ping_loop.await;
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

fn command_line() -> Probe {
    boxed(async {
        use clap::Parser;
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
            crate::cli::HubCli::try_parse_from(["claudecord-hub", "nonsense"]).is_err(),
            "an unknown command was accepted"
        );
        Ok("every hub command parses, unknown commands refused".into())
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
        let mut ws = connect_ws(&format!("ws://{}/api/v1/node/connect", hub.addr), &token)
            .await
            .map_err(|e| e.to_string())?;
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
        let reg = serde_json::to_string(&crate::protocol::NodeFrame::AgentRegister {
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
        .unwrap();
        ws.send(Message::Text(reg.into()))
            .await
            .map_err(|e| e.to_string())?;
        ensure!(
            eventually(async || hub
                .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
                .await
                .unwrap_or(false))
            .await,
            "the agent never registered"
        );
        let say = serde_json::to_string(&crate::protocol::NodeFrame::AgentSay {
            agent_id: "demo/otter".into(),
            text: "hello team".into(),
            thread: None,
            say_id: None,
        })
        .unwrap();
        ws.send(Message::Text(say.into()))
            .await
            .map_err(|e| e.to_string())?;
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
            while let Some(msg) = ws.next().await {
                if let Ok(Message::Text(t)) = msg
                    && let Ok(crate::protocol::HubFrame::Deliver { ref text, .. }) =
                        serde_json::from_str(&t)
                    && text == "from discord"
                {
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
