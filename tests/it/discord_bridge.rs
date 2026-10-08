//! The Discord bridge, against a stand-in Discord (its REST API and its live gateway) and a real hub with a real device
//! link. Covers both directions: what the hub wants shown becomes the right Discord calls, and what happens in Discord
//! becomes the right calls on the hub, only ever as a named person with the right role.

use claudecord::device::link::{self, LinkEvent, LinkOpts};
use claudecord::discord::bridge::{self, BridgeConfig};
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use claudecord::server::{self, Config as ServerConfig};
use claudecord::store::Store;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

use claudecord::discord::fake::{Fake, Log, start_fake};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-dis-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Rig {
    fake: Fake,
    addr: String,
    hub: server::Hub,
    device: mpsc::Receiver<LinkEvent>,
    link: link::Link,
    db: std::path::PathBuf,
}

impl Rig {
    /// Waits until the stand-in Discord has a record matching `f`, and returns it.
    async fn until(&self, what: &str, f: impl Fn(&Log) -> Option<Value>) -> Value {
        for _ in 0..200 {
            if let Some(v) = f(&self.fake.log.lock().unwrap()) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Sends a gateway event to the bridge.
    fn event(&self, name: &str, data: Value) {
        let _ = self
            .fake
            .events
            .send(json!({"op": 0, "s": 2, "t": name, "d": data}).to_string());
    }

    /// Waits for a frame from the hub on the device that matches.
    async fn frame(&mut self, what: &str, f: impl Fn(&HubFrame) -> bool) -> HubFrame {
        let r = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(e) = self.device.recv().await {
                if let LinkEvent::Frame(fr) = e
                    && f(&fr)
                {
                    return Some(fr);
                }
            }
            None
        })
        .await;
        r.ok()
            .flatten()
            .unwrap_or_else(|| panic!("no frame: {what}"))
    }

    /// True if no frame matching `f` arrives for a short while.
    async fn quiet(&mut self, f: impl Fn(&HubFrame) -> bool) -> bool {
        let r = tokio::time::timeout(Duration::from_millis(600), async {
            while let Some(e) = self.device.recv().await {
                if let LinkEvent::Frame(fr) = e
                    && f(&fr)
                {
                    return true;
                }
            }
            false
        })
        .await;
        !r.unwrap_or(false)
    }
}

async fn rig(name: &str) -> Rig {
    rig_with(name, 0).await
}

/// The same, with Discord refusing the bridge's first `me_failures` "who am I" calls.
async fn rig_with(name: &str, me_failures: usize) -> Rig {
    let (fake, addr) = start_fake().await;
    fake.me_failures
        .store(me_failures, std::sync::atomic::Ordering::SeqCst);
    let dir = tmp(name);
    let db = dir.join("hub.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    let hub = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ping_every: Duration::from_millis(300),
            tick_every: Duration::from_millis(100),
            ..ServerConfig::default()
        },
        HubCore::default(),
        store,
    )
    .await
    .unwrap();
    let (tx, device) = mpsc::channel(256);
    let link = link::spawn(
        format!("ws://{}/api/v1/node/connect", hub.addr),
        token,
        "mac".into(),
        tx,
        LinkOpts {
            ping_every: Duration::from_millis(300),
            connect_timeout: Duration::from_secs(1),
            backoff_min: Duration::from_millis(30),
            backoff_max: Duration::from_millis(200),
            proxy: None,
        },
    );
    let cfg = BridgeConfig {
        scope: None,
        api_base: format!("http://{addr}/api"),
        token: "bottoken".into(),
        guild: "g1".into(),
        gateway_url: None,
        db_path: db.clone(),
        owners: vec![],
        backoff_max: Duration::from_millis(300),
    };
    bridge::spawn(hub.handle(), hub.chat(), cfg);
    let r = Rig {
        fake,
        addr,
        hub,
        device,
        link,
        db: db.clone(),
    };
    r.until("gateway identified", |l| {
        (l.identifies >= 1).then_some(json!(true))
    })
    .await;
    r
}

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        agent_id: format!("demo/{name}"),
        name: name.into(),
        project: "demo".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    }
}

async fn register(r: &Rig, name: &str) {
    r.link
        .send(NodeFrame::AgentRegister {
            agent: spec(name),
            cwd: "/x".into(),
        })
        .await;
    for _ in 0..100 {
        if r.hub
            .call({
                let n = format!("demo/{name}");
                move |c, _| (c.agent(&n).is_some(), vec![])
            })
            .await
            .unwrap()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("agent did not register");
}

/// The channel id the bridge made for project demo.
async fn channel(r: &Rig) -> String {
    r.until("project channel", |l| {
        l.channels
            .iter()
            .find(|c| c["name"] == "demo")
            .map(|c| c["id"].clone())
    })
    .await
    .as_str()
    .unwrap()
    .to_string()
}

fn message(channel: &str, id: &str, user: &str, content: &str) -> Value {
    json!({"id": id, "channel_id": channel, "content": content, "author": {"id": user, "username": format!("user{user}")}, "attachments": []})
}

#[tokio::test(flavor = "multi_thread")]
async fn on_start_the_owner_is_known_and_the_slash_commands_are_registered() {
    let r = rig("start").await;
    let cmds = r.until("commands", |l| l.commands.clone()).await;
    let names: Vec<&str> = cmds
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for want in [
        "agents", "pause", "resume", "stop", "killall", "btw", "grant", "role", "raw", "spawn",
        "dump", "pickup", "devices",
    ] {
        assert!(names.contains(&want), "missing /{want}");
    }
    assert_eq!(
        r.hub
            .call(|c, _| (c.role_of("demo", "1"), vec![]))
            .await
            .unwrap(),
        Some(Role::Owner),
        "the bot's owner became the hub's owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_words_appear_under_its_own_name_in_a_channel_and_in_a_thread() {
    let r = rig("post").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    r.link
        .send(NodeFrame::AgentSay {
            agent_id: "demo/otter".into(),
            text: "hello team".into(),
            thread: None,
            say_id: None,
        })
        .await;
    let post = r
        .until("webhook post", |l| {
            l.posts
                .iter()
                .find(|p| p["content"] == "hello team")
                .cloned()
        })
        .await;
    assert_eq!(post["username"], "otter");
    assert_eq!(post["channel"], ch.as_str());
    r.link
        .send(NodeFrame::AgentSay {
            agent_id: "demo/otter".into(),
            text: "in a thread".into(),
            thread: Some("T1".into()),
            say_id: None,
        })
        .await;
    let post = r
        .until("threaded post", |l| {
            l.posts
                .iter()
                .find(|p| p["content"] == "in a thread")
                .cloned()
        })
        .await;
    assert!(
        post["thread_id"].is_string(),
        "posted into a Discord thread: {post}"
    );
}

fn deliver_text(f: &HubFrame, want: &str) -> bool {
    matches!(f, HubFrame::Deliver { text, .. } if text == want)
}

fn interaction(kind: u64, channel: &str, user: &str, data: Value) -> Value {
    json!({"id": format!("i{}", data), "token": "itok", "type": kind, "channel_id": channel, "member": {"user": {"id": user, "username": format!("user{user}")}}, "data": data})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_from_a_listed_person_reaches_the_agent_and_acceptance_gets_a_check_mark() {
    let mut r = rig("human").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    r.event("MESSAGE_CREATE", message(&ch, "m1", "1", "please start"));
    let frame = r
        .frame("the message", |f| deliver_text(f, "please start"))
        .await;
    let HubFrame::Deliver { from, msg_id, .. } = frame else {
        unreachable!()
    };
    assert_eq!(from, "user1 (owner)", "sent as a named person with a role");
    r.link
        .send(NodeFrame::AgentAccepted {
            agent_id: "demo/otter".into(),
            msg_ids: vec![msg_id.unwrap()],
        })
        .await;
    let reaction = r
        .until("check mark", |l| {
            l.reactions
                .iter()
                .find(|x| x["message"] == "m1" && x["emoji"] == "\u{2705}")
                .cloned()
        })
        .await;
    assert_eq!(reaction["channel"], ch.as_str());
    assert_eq!(reaction["emoji"], "\u{2705}", "{reaction}");
    // The eyes that said "received" are taken off once the check mark is there.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let left: Vec<String> = r
        .fake
        .log
        .lock()
        .unwrap()
        .reactions
        .iter()
        .filter(|x| x["message"] == "m1")
        .map(|x| x["emoji"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        left,
        vec!["\u{2705}".to_string()],
        "only the check mark stays"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_webhooks_strangers_and_viewers_cannot_instruct_agents() {
    let mut r = rig("ignored").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    // Warm up so the agent has its brief and any later delivery is only the message in question.
    r.event("MESSAGE_CREATE", message(&ch, "m0", "1", "hello"));
    r.frame("warm up", |f| deliver_text(f, "hello")).await;
    let mut bot = message(&ch, "m1", "7", "from a bot");
    bot["author"]["bot"] = json!(true);
    r.event("MESSAGE_CREATE", bot);
    let mut hook = message(&ch, "m2", "7", "from a webhook");
    hook["webhook_id"] = json!("w1");
    r.event("MESSAGE_CREATE", hook);
    r.event(
        "MESSAGE_CREATE",
        message(&ch, "m3", "99", "from a stranger"),
    );
    assert!(
        r.quiet(|f| matches!(f, HubFrame::Deliver { text, .. } if text.starts_with("from a")))
            .await,
        "nothing from bots, webhooks or strangers was delivered"
    );
    assert!(
        r.fake
            .log
            .lock()
            .unwrap()
            .reactions
            .iter()
            .all(|x| x["message"] != "m3"),
        "a stranger gets no reaction at all"
    );
    // A viewer is known but may not instruct: refused with a visible sign.
    r.hub
        .call(|c, _| {
            let kd = Human {
                id: "1".into(),
                name: "kd".into(),
            };
            (
                c.set_role(
                    &kd,
                    "demo",
                    &Human {
                        id: "5".into(),
                        name: "vi".into(),
                    },
                    Some(Role::Viewer),
                )
                .unwrap(),
                vec![],
            )
        })
        .await;
    r.event("MESSAGE_CREATE", message(&ch, "m4", "5", "viewer words"));
    let refused = r
        .until("refusal sign", |l| {
            l.reactions.iter().find(|x| x["message"] == "m4").cloned()
        })
        .await;
    assert_eq!(refused["emoji"], "\u{26D4}");
    assert!(r.quiet(|f| deliver_text(f, "viewer words")).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_with_buttons_is_answered_by_a_button_or_by_replying_and_then_closed() {
    let mut r = rig("ask").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    r.link
        .send(NodeFrame::AgentAsk {
            agent_id: "demo/otter".into(),
            ask_id: "a1".into(),
            question: "which db?".into(),
            options: Some(vec!["postgres".into(), "sqlite".into()]),
            thread: None,
        })
        .await;
    let msg = r
        .until("the question", |l| {
            l.messages
                .iter()
                .find(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("which db?"))
                })
                .cloned()
        })
        .await;
    let buttons = msg["components"][0]["components"].as_array().unwrap();
    assert_eq!(buttons.len(), 2);
    assert_eq!(buttons[1]["custom_id"], "ask:demo:Q1:1");
    // The owner presses "sqlite".
    r.event(
        "INTERACTION_CREATE",
        interaction(3, &ch, "1", json!({"custom_id": "ask:demo:Q1:1"})),
    );
    r.frame(
        "the answer",
        |f| matches!(f, HubFrame::Answer { text, .. } if text == "sqlite"),
    )
    .await;
    let edit = r
        .until("the question closed", |l| {
            l.edits
                .iter()
                .find(|e| e["content"].as_str().is_some_and(|c| c.starts_with("Q1:")))
                .cloned()
        })
        .await;
    assert!(
        edit["content"].as_str().unwrap().contains("answered by"),
        "{edit}"
    );
    assert_eq!(edit["components"], json!([]), "the buttons are gone");
    // A second question is answered by replying to its message.
    r.link
        .send(NodeFrame::AgentAsk {
            agent_id: "demo/otter".into(),
            ask_id: "a2".into(),
            question: "which port?".into(),
            options: None,
            thread: None,
        })
        .await;
    let q2 = r
        .until("second question", |l| {
            l.messages
                .iter()
                .find(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("which port?"))
                })
                .cloned()
        })
        .await;
    let mut reply = message(&ch, "m9", "1", "8080");
    reply["message_reference"] = json!({"message_id": q2["id"]});
    r.event("MESSAGE_CREATE", reply);
    r.frame(
        "the typed answer",
        |f| matches!(f, HubFrame::Answer { text, .. } if text == "8080"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_buttons_are_checked_against_roles() {
    let mut r = rig("perm").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    r.hub
        .call(|c, _| {
            let kd = Human {
                id: "1".into(),
                name: "kd".into(),
            };
            (
                c.set_role(
                    &kd,
                    "demo",
                    &Human {
                        id: "2".into(),
                        name: "sam".into(),
                    },
                    Some(Role::Operator),
                )
                .unwrap(),
                vec![],
            )
        })
        .await;
    r.link
        .send(NodeFrame::AgentPermission {
            agent_id: "demo/otter".into(),
            perm_id: "x1".into(),
            kind: "edit".into(),
            action: "src/a.rs".into(),
            thread: None,
        })
        .await;
    let msg = r
        .until("the request", |l| {
            l.messages
                .iter()
                .find(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("wants permission"))
                })
                .cloned()
        })
        .await;
    let ids: Vec<&str> = msg["components"][0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "perm:demo:P1:deny",
            "perm:demo:P1:once",
            "perm:demo:P1:kind",
            "perm:demo:P1:all"
        ]
    );
    // An operator may not make a standing grant.
    r.event(
        "INTERACTION_CREATE",
        interaction(3, &ch, "2", json!({"custom_id": "perm:demo:P1:all"})),
    );
    let denied = r
        .until("the refusal", |l| {
            l.responses
                .iter()
                .find(|x| x["content"].as_str().is_some_and(|c| c.contains("owner")))
                .cloned()
        })
        .await;
    assert!(
        denied["content"]
            .as_str()
            .unwrap()
            .contains("needs the owner role")
    );
    assert!(
        r.quiet(|f| matches!(f, HubFrame::Decision { .. })).await,
        "nothing was decided"
    );
    // The same operator may allow it once.
    r.event(
        "INTERACTION_CREATE",
        interaction(3, &ch, "2", json!({"custom_id": "perm:demo:P1:once"})),
    );
    r.frame("the decision", |f| {
        matches!(f, HubFrame::Decision { allow: true, .. })
    })
    .await;
    r.until("the request closed", |l| {
        l.edits
            .iter()
            .find(|e| {
                e["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("P1:") && c.contains("allowed once"))
            })
            .cloned()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_commands_run_with_the_callers_role_and_nothing_more() {
    let mut r = rig("slash").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let say =
        |name: &str, opts: Value| interaction(2, &ch, "1", json!({"name": name, "options": opts}));
    // /agents lists who is here.
    r.event("INTERACTION_CREATE", say("agents", json!([])));
    let reply = r
        .until("agents reply", |l| {
            l.responses
                .iter()
                .find(|x| x["content"].as_str().is_some_and(|c| c.contains("otter")))
                .cloned()
        })
        .await;
    assert_eq!(reply["type"], 4);
    // /role makes sam an operator, then sam is heard.
    r.event("INTERACTION_CREATE", say("role", json!([{"name": "user", "type": 6, "value": "2"}, {"name": "role", "type": 3, "value": "operator"}])));
    r.until("role reply", |l| {
        l.responses
            .iter()
            .find(|x| {
                x["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("is now operator"))
            })
            .cloned()
    })
    .await;
    assert_eq!(
        r.hub
            .call(|c, _| (c.role_of("demo", "2"), vec![]))
            .await
            .unwrap(),
        Some(Role::Operator)
    );
    // /raw by the owner types exactly what was given.
    r.event("INTERACTION_CREATE", say("raw", json!([{"name": "agent", "type": 3, "value": "otter"}, {"name": "text", "type": 3, "value": "/compact"}])));
    r.frame(
        "the raw text",
        |f| matches!(f, HubFrame::Raw { text, .. } if text == "/compact"),
    )
    .await;
    // /raw by an operator is refused.
    let mut by_sam = interaction(
        2,
        &ch,
        "2",
        json!({"name": "raw", "options": [{"name": "agent", "type": 3, "value": "otter"}, {"name": "text", "type": 3, "value": "/clear"}]}),
    );
    by_sam["id"] = json!("i-sam");
    r.event("INTERACTION_CREATE", by_sam);
    r.until("refusal", |l| {
        l.responses
            .iter()
            .find(|x| x["interaction"] == "i-sam")
            .cloned()
    })
    .await;
    assert!(
        r.quiet(|f| matches!(f, HubFrame::Raw { text, .. } if text == "/clear"))
            .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_reaches_the_agent_as_a_file() {
    let mut r = rig("file").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let mut m = message(&ch, "m5", "1", "see this");
    m["attachments"] = json!([{"id": "att1", "filename": "plan.txt", "size": 16, "url": format!("http://{}/files/plan.txt", r.addr)}]);
    r.event("MESSAGE_CREATE", m);
    let f = r
        .frame(
            "file chunk",
            |f| matches!(f, HubFrame::FileChunk { name, .. } if name == "plan.txt"),
        )
        .await;
    let HubFrame::FileChunk { last, from, .. } = f else {
        unreachable!()
    };
    assert!(last);
    assert_eq!(from, "user1 (owner)");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_reply_is_attached_as_a_file_instead_of_being_cut() {
    let r = rig("long").await;
    register(&r, "otter").await;
    let _ = channel(&r).await;
    let long = "word ".repeat(600);
    r.link
        .send(NodeFrame::AgentSay {
            agent_id: "demo/otter".into(),
            text: long,
            thread: None,
            say_id: None,
        })
        .await;
    let post = r
        .until("the long post", |l| {
            l.posts.iter().find(|p| p["file"] == "message.txt").cloned()
        })
        .await;
    assert!(
        post["content"]
            .as_str()
            .unwrap()
            .contains("full text is attached")
    );
    assert!(post["content"].as_str().unwrap().len() < 2000);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bridge_reconnects_to_discord_by_itself_and_keeps_working() {
    let mut r = rig("reconnect").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let _ = r.fake.kick.send(());
    r.until("a second gateway connection", |l| {
        (l.gateway_connects >= 2).then_some(json!(true))
    })
    .await;
    r.until("resumed rather than starting over", |l| {
        (l.resumes >= 1).then_some(json!(true))
    })
    .await;
    // The drop and the recovery are both in the uptime log: up, then down, then up again.
    let log = Store::open(&r.db, None).unwrap();
    let mut states = vec![];
    for _ in 0..100 {
        states = log
            .uptime_changes("discord", 0)
            .unwrap()
            .iter()
            .map(|c| c.state.as_str())
            .collect();
        if states.ends_with(&["up", "down", "up"]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(states.ends_with(&["up", "down", "up"]), "{states:?}");
    r.event("MESSAGE_CREATE", message(&ch, "m7", "1", "after the drop"));
    r.frame("a message after reconnecting", |f| {
        deliver_text(f, "after the drop")
    })
    .await;
}

#[tokio::test]
async fn a_discord_outage_at_start_up_only_delays_the_bridge_it_does_not_leave_it_half_set_up() {
    // Discord refuses the first two "who am I" calls; the bridge keeps trying and then comes up fully, owner and commands included.
    let r = rig_with("outage", 2).await;
    r.until("slash commands registered after the outage", |l| {
        l.commands.is_some().then_some(json!(true))
    })
    .await;
    let owner = r
        .hub
        .call(|c, _| (c.role_of("demo", "1"), vec![]))
        .await
        .unwrap();
    assert_eq!(
        owner,
        Some(claudecord::hub::Role::Owner),
        "the bot's owner was found once Discord answered"
    );
    assert_eq!(
        r.fake.me_failures.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// A Discord message id for "now" plus `plus` (ids are numbers that rise with time; the bridge watches a channel from about a minute ago).
fn snow(plus: u64) -> String {
    ((((claudecord::now_ms() - 1_420_070_400_000).max(0)) as u64) << 22 | 1)
        .wrapping_add(plus)
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_arrives_twice_is_taken_once() {
    let mut r = rig("twice").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let m = message(&ch, &snow(10), "1", "only once please");
    r.event("MESSAGE_CREATE", m.clone());
    r.frame("the message", |f| deliver_text(f, "only once please"))
        .await;
    // A resumed connection replays it, and Discord is read back the same message: it is not taken a second time.
    r.event("MESSAGE_CREATE", m);
    assert!(
        r.quiet(|f| deliver_text(f, "only once please")).await,
        "the same message was delivered twice"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn what_was_said_while_the_bridge_was_not_connected_is_read_back_once_and_in_order() {
    let mut r = rig("backfill").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    // One message arrives live and is taken.
    r.event("MESSAGE_CREATE", message(&ch, &snow(10), "1", "live one"));
    r.frame("the live message", |f| deliver_text(f, "live one"))
        .await;
    // The connection drops and Discord has forgotten the session, so the bridge will have to start a new one. While it was away, two
    // people spoke (and a bot, which is ignored); the live message is in Discord's record too, and must not be taken again.
    r.fake
        .reject_resume
        .store(true, std::sync::atomic::Ordering::SeqCst);
    {
        let mut log = r.fake.log.lock().unwrap();
        let mut bot = message(&ch, &snow(40), "9", "a bot says hi");
        bot["author"]["bot"] = serde_json::json!(true);
        log.history.insert(
            ch.clone(),
            vec![
                message(&ch, &snow(10), "1", "live one"),
                message(&ch, &snow(20), "1", "while away 1"),
                message(&ch, &snow(30), "1", "while away 2"),
                bot,
            ],
        );
    }
    let _ = r.fake.kick.send(());
    r.until("a fresh session, not a resume", |l| {
        (l.identifies >= 2).then_some(json!(true))
    })
    .await;
    r.frame("first message sent while away", |f| {
        deliver_text(f, "while away 1")
    })
    .await;
    r.frame("second message sent while away", |f| {
        deliver_text(f, "while away 2")
    })
    .await;
    assert!(
        r.quiet(|f| deliver_text(f, "live one") || deliver_text(f, "a bot says hi"))
            .await,
        "a message already taken, or a bot's, was delivered"
    );
    // And the read-back is itself safe to repeat: another fresh session takes nothing twice.
    let _ = r.fake.kick.send(());
    r.until("a third session", |l| {
        (l.identifies >= 3).then_some(json!(true))
    })
    .await;
    assert!(
        r.quiet(|f| deliver_text(f, "while away 1") || deliver_text(f, "while away 2"))
            .await,
        "read-back took messages a second time"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_agent_gets_a_mentionable_role_and_picking_it_from_the_list_reaches_that_agent() {
    let mut r = rig("roles").await;
    register(&r, "otter").await;
    register(&r, "macbook-eeg-main").await;
    let ch = channel(&r).await;
    // A role named after the agent, anyone can mention it, so typing @macbook-eeg-main offers it in Discord.
    let role = r
        .until("a role for the hyphenated agent", |l| {
            l.roles
                .iter()
                .find(|x| x["name"] == "macbook-eeg-main (agent)")
                .cloned()
        })
        .await;
    assert_eq!(role["mentionable"], true);
    let id = role["id"].as_str().unwrap().to_string();
    // Picking it sends `<@&id>`; the agent receives the plain @name, and only that agent is addressed.
    r.event(
        "MESSAGE_CREATE",
        message(&ch, "m9", "1", &format!("<@&{id}> run the tests")),
    );
    r.frame("the mention as a plain @name", |f| {
        deliver_text(f, "@macbook-eeg-main run the tests")
    })
    .await;
    // A mention of some other role is left alone, and registering the same agents again makes no second role.
    register(&r, "otter").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let roles = r.fake.log.lock().unwrap().roles.clone();
    assert_eq!(
        roles
            .iter()
            .filter(|x| x["name"] == "otter (agent)")
            .count(),
        1,
        "{roles:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_is_acknowledged_at_once_and_when_nobody_is_there_to_get_it_that_is_said() {
    let r = rig("ack").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    // Seen by the hub and queued for the agent: an eyes reaction straight away (the check mark waits for the agent to take it up).
    r.event("MESSAGE_CREATE", message(&ch, "m1", "1", "are you there"));
    let seen = r
        .until("the eyes", |l| {
            l.reactions.iter().find(|x| x["message"] == "m1").cloned()
        })
        .await;
    assert_eq!(seen["emoji"], "\u{1F440}", "{seen}");
    // The agent leaves; the next message has nobody to go to, and the channel is told so in words.
    r.link
        .send(NodeFrame::AgentGone {
            agent_id: "demo/otter".into(),
        })
        .await;
    for _ in 0..100 {
        if !r
            .hub
            .call(|c, _| (c.agent("demo/otter").is_some(), vec![]))
            .await
            .unwrap()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    r.event("MESSAGE_CREATE", message(&ch, "m2", "1", "anyone"));
    let said = r
        .until("the explanation", |l| {
            l.messages
                .iter()
                .find(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("Nobody is running"))
                })
                .cloned()
        })
        .await;
    assert_eq!(said["channel"], ch.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_can_really_tag_a_person_and_a_reply_to_an_agents_message_goes_to_that_agent() {
    let mut r = rig("tags").await;
    register(&r, "otter").await;
    register(&r, "heron").await;
    let ch = channel(&r).await;
    // The person speaks once, so the bridge learns what to call them.
    r.event("MESSAGE_CREATE", message(&ch, "m1", "1", "hello"));
    r.frame("hello", |f| deliver_text(f, "hello")).await;
    // An agent addresses them by name: a real tag, and only that person is allowed to be notified.
    r.link
        .send(NodeFrame::AgentSay {
            agent_id: "demo/heron".into(),
            text: "@user1 the build is done".into(),
            thread: None,
            say_id: None,
        })
        .await;
    let post = r
        .until("the tagged post", |l| {
            l.posts
                .iter()
                .find(|p| {
                    p["username"] == "heron"
                        && p["content"]
                            .as_str()
                            .is_some_and(|c| c.contains("build is done"))
                })
                .cloned()
        })
        .await;
    assert_eq!(post["content"], "<@1> the build is done", "{post}");
    assert_eq!(post["notify"], json!(["1"]), "{post}");
    // An agent addressed by name is tagged with its role, and that role is allowed to be pinged.
    let role_id = r
        .until("otter's role", |l| {
            l.roles
                .iter()
                .find(|x| x["name"] == "otter (agent)")
                .map(|x| x["id"].clone())
        })
        .await;
    r.link
        .send(NodeFrame::AgentSay {
            agent_id: "demo/heron".into(),
            text: "@otter please check".into(),
            thread: None,
            say_id: None,
        })
        .await;
    let ping = r
        .until("the role ping", |l| {
            l.posts
                .iter()
                .find(|p| {
                    p["content"]
                        .as_str()
                        .is_some_and(|c| c.ends_with("please check"))
                })
                .cloned()
        })
        .await;
    let id = role_id.as_str().unwrap();
    assert_eq!(ping["content"], format!("<@&{id}> please check"), "{ping}");
    assert_eq!(ping["notify_roles"], json!([id]), "{ping}");
    // Replying to that message, with no name in the reply, goes to heron and not to the lead (otter).
    let mut reply = message(&ch, "m2", "1", "thanks, ship it");
    reply["message_reference"] = json!({"message_id": post["id"]});
    r.event("MESSAGE_CREATE", reply);
    r.frame("the reply", |f| {
        matches!(f, HubFrame::Deliver { agent_id, text, .. } if agent_id == "demo/heron" && text == "@heron thanks, ship it")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_deletes_the_chat_keeps_the_pinned_ones_and_starts_every_agent_over() {
    let mut r = rig("clear").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    // Some messages are in the channel, one of them pinned (and so kept).
    let base = ((claudecord::now_ms() - 1_420_070_400_000) as u64) << 22;
    let id = |n: u64| base + n;
    let msgs: Vec<serde_json::Value> = (1..=4)
        .map(|n| {
            let mut m = message(&ch, &id(n).to_string(), "1", &format!("old {n}"));
            if n == 2 {
                m["pinned"] = json!(true);
            }
            m
        })
        .collect();
    r.fake.log.lock().unwrap().history.insert(ch.clone(), msgs);
    r.event(
        "INTERACTION_CREATE",
        interaction(
            2,
            &ch,
            "1",
            json!({"name": "clear", "options": [{"name": "chat", "type": 1, "options": []}]}),
        ),
    );
    // The agent is told to start again from its base.
    r.frame(
        "restart",
        |f| matches!(f, HubFrame::Restart { agent_id } if agent_id == "demo/otter"),
    )
    .await;
    // The channel's messages are deleted, the pinned one is not, and the new chat is announced.
    let said = r
        .until("the new chat", |l| {
            l.messages
                .iter()
                .find(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("New chat"))
                })
                .cloned()
        })
        .await;
    assert_eq!(said["channel"], ch.as_str());
    let mut deleted = r.fake.log.lock().unwrap().deleted.clone();
    deleted.sort();
    let want: Vec<String> = [1, 3, 4].iter().map(|n| id(*n).to_string()).collect();
    assert_eq!(deleted, want, "everything but the pinned message");
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_with_an_agent_named_starts_only_that_one_over_and_leaves_the_channel_alone() {
    let mut r = rig("clearone").await;
    register(&r, "otter").await;
    register(&r, "heron").await;
    let ch = channel(&r).await;
    let base = ((claudecord::now_ms() - 1_420_070_400_000) as u64) << 22;
    r.fake.log.lock().unwrap().history.insert(
        ch.clone(),
        vec![message(&ch, &(base + 1).to_string(), "1", "keep me")],
    );
    r.event(
        "INTERACTION_CREATE",
        interaction(
            2,
            &ch,
            "1",
            json!({"name": "clear", "options": [{"name": "agent", "type": 1, "options": [{"name": "agent", "value": "heron"}]}]}),
        ),
    );
    r.frame(
        "heron restarts",
        |f| matches!(f, HubFrame::Restart { agent_id } if agent_id == "demo/heron"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = r.fake.log.lock().unwrap();
    assert!(
        log.deleted.is_empty(),
        "the channel is left alone: {:?}",
        log.deleted
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_clear_starts_the_lead_over_and_nothing_else() {
    let mut r = rig("clearlead").await;
    register(&r, "otter").await;
    register(&r, "heron").await;
    let ch = channel(&r).await;
    r.event(
        "INTERACTION_CREATE",
        interaction(
            2,
            &ch,
            "1",
            json!({"name": "clear", "options": [{"name": "agent", "type": 1, "options": []}]}),
        ),
    );
    // otter registered first, so it leads: it restarts, the channel is left alone.
    r.frame(
        "the lead restarts",
        |f| matches!(f, HubFrame::Restart { agent_id } if agent_id == "demo/otter"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(r.fake.log.lock().unwrap().deleted.is_empty());
}

#[test]
fn every_slash_command_fits_discords_limits_or_discord_refuses_the_whole_list() {
    // One command over a limit makes Discord reject ALL of them, so no command shows up. Names: 1-32 lower-case; descriptions: 1-100.
    let defs = claudecord::discord::commands::definitions();
    let mut bad = Vec::new();
    for c in defs.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let desc = c["description"].as_str().unwrap();
        if !(1..=32).contains(&name.len()) || name != name.to_lowercase() {
            bad.push(format!("command name {name:?}"));
        }
        if !(1..=100).contains(&desc.chars().count()) {
            bad.push(format!(
                "/{name} description is {} characters",
                desc.chars().count()
            ));
        }
        for o in c["options"].as_array().into_iter().flatten() {
            let (on, od) = (
                o["name"].as_str().unwrap(),
                o["description"].as_str().unwrap(),
            );
            if !(1..=32).contains(&on.len()) || on != on.to_lowercase() {
                bad.push(format!("/{name} option name {on:?}"));
            }
            if !(1..=100).contains(&od.chars().count()) {
                bad.push(format!(
                    "/{name} {on}: description is {} characters",
                    od.chars().count()
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lead_chooses_who_leads_and_the_bridge_says_so() {
    let r = rig("lead").await;
    register(&r, "otter").await;
    register(&r, "heron").await;
    let ch = channel(&r).await;
    r.event(
        "INTERACTION_CREATE",
        interaction(
            2,
            &ch,
            "1",
            json!({"name": "lead", "options": [{"name": "agent", "value": "heron"}]}),
        ),
    );
    r.until("the announcement", |l| {
        l.messages
            .iter()
            .find(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("heron now leads"))
            })
            .cloned()
    })
    .await;
    assert!(
        r.hub
            .call(|c, _| (c.agent("demo/heron").unwrap().is_lead, vec![]))
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_an_agent_option_lists_the_agents_here_that_match() {
    let r = rig("typing").await;
    register(&r, "otter").await;
    register(&r, "heron").await;
    let ch = channel(&r).await;
    r.event(
        "INTERACTION_CREATE",
        interaction(
            4,
            &ch,
            "1",
            json!({"name": "raw", "options": [{"name": "agent", "type": 3, "value": "he", "focused": true}]}),
        ),
    );
    let got = r
        .until("the list", |l| {
            l.responses.iter().find(|x| x["type"] == 8).cloned()
        })
        .await;
    let names: Vec<&str> = got["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["heron"]);
    // Every command option that names an agent asks for this list.
    let defs = claudecord::discord::commands::definitions().to_string();
    assert!(defs.contains("\"autocomplete\":true"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_roles_of_agents_that_are_gone_are_deleted_by_themselves() {
    let r = rig("prune").await;
    register(&r, "otter").await;
    let role = r
        .until("otter's role", |l| {
            l.roles
                .iter()
                .find(|x| x["name"] == "otter (agent)")
                .cloned()
        })
        .await;
    // A role left behind by an agent that no longer exists (an earlier run, or one that was removed), and one that is not ours.
    {
        let mut l = r.fake.log.lock().unwrap();
        l.roles.push(json!({"id": "900", "name": "ghost (agent)", "guild": role["guild"], "mentionable": true}));
        l.roles.push(
            json!({"id": "901", "name": "Moderators", "guild": role["guild"], "mentionable": true}),
        );
    }
    register(&r, "heron").await;
    r.until("the ghost role to go", |l| {
        l.deleted_roles
            .iter()
            .find(|x| *x == "900")
            .map(|x| json!(x))
    })
    .await;
    let l = r.fake.log.lock().unwrap();
    assert!(
        !l.deleted_roles
            .iter()
            .any(|x| *x == "901" || *x == role["id"].as_str().unwrap()),
        "{:?}",
        l.deleted_roles
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_post_nothing_extra_in_the_channel_around_them() {
    let r = rig("audit").await;
    for n in ["otter", "heron", "ibis"] {
        register(&r, n).await;
    }
    let ch = channel(&r).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let before = r.fake.log.lock().unwrap().messages.len();
    let run = |name: &str, opts: Value| {
        r.event(
            "INTERACTION_CREATE",
            interaction(2, &ch, "1", json!({"name": name, "options": opts})),
        );
    };
    for (n, o) in [
        ("pause", json!([])),
        ("resume", json!([])),
        ("stop", json!([{"name": "agent", "value": "heron"}])),
        ("btw", json!([{"name": "text", "value": "hi"}])),
        ("dump", json!([])),
        ("lead", json!([{"name": "agent", "value": "ibis"}])),
        ("killall", json!([])),
    ] {
        run(n, o);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    for n in ["otter", "heron", "ibis"] {
        r.link
            .send(NodeFrame::AgentGone {
                agent_id: format!("demo/{n}"),
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let l = r.fake.log.lock().unwrap();
    let posted: Vec<String> = l
        .messages
        .iter()
        .skip(before)
        .map(|m| m["content"].to_string())
        .collect();
    // Commands answer the person who ran them (privately) and post nothing else; only /lead says so to everyone, because it changes who the agents report to.
    assert_eq!(
        posted,
        vec!["\"ibis now leads demo.\""],
        "extra posts around the commands: {posted:#?}"
    );
    assert!(
        l.posts.is_empty(),
        "no agent posted anything: {:?}",
        l.posts
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_that_cannot_be_fetched_is_said_so_and_the_message_still_goes_through() {
    let mut r = rig("nofile").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let mut m = message(&ch, "m6", "1", "see this");
    // Nothing listens on port 1.
    m["attachments"] = json!([{"id": "att2", "filename": "plan.txt", "size": 16, "url": "http://127.0.0.1:1/files/plan.txt"}]);
    r.event("MESSAGE_CREATE", m);
    r.until("the notice", |l| {
        l.messages
            .iter()
            .find(|x| {
                x["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("Could not fetch plan.txt"))
            })
            .cloned()
    })
    .await;
    r.frame("the text itself", |f| deliver_text(f, "see this"))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_asks_the_terminal_and_leaves_no_message_of_its_own() {
    let mut r = rig("screenask").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let mut i = interaction(
        2,
        &ch,
        "1",
        json!({"name": "screen", "options": [{"name": "agent", "type": 3, "value": "otter"}]}),
    );
    i["application_id"] = json!("app1");
    r.event("INTERACTION_CREATE", i);
    r.frame("the screen request", |f| {
        matches!(f, HubFrame::Screen { .. })
    })
    .await;
    // Discord is answered (a deferred, private acknowledgement) and that acknowledgement is taken away again: no "Asking ..." text anywhere.
    let ack = r
        .until("the acknowledgement", |l| {
            l.responses.iter().find(|x| x["type"] == 5).cloned()
        })
        .await;
    assert!(ack["content"].is_null(), "{ack}");
    r.until("the acknowledgement taken away", |l| {
        l.deleted
            .iter()
            .find(|d| d.starts_with("@original"))
            .map(|d| json!(d))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn move_files_an_agent_under_another_project_and_says_so() {
    let mut r = rig("moveslash").await;
    register(&r, "otter").await;
    let ch = channel(&r).await;
    let say =
        |name: &str, opts: Value| interaction(2, &ch, "1", json!({"name": name, "options": opts}));
    r.event(
        "INTERACTION_CREATE",
        say("move", json!([{"name": "agent", "type": 3, "value": "otter"}, {"name": "project", "type": 3, "value": "other"}])),
    );
    let reply = r
        .until("the answer", |l| {
            l.responses
                .iter()
                .find(|x| {
                    x["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("moved to other"))
                })
                .cloned()
        })
        .await;
    assert_eq!(reply["type"], 4);
    let there = r
        .hub
        .call(|c, _| {
            (
                c.find_by_name("other", "otter").is_some()
                    && c.find_by_name("demo", "otter").is_none(),
                vec![],
            )
        })
        .await
        .unwrap();
    assert!(
        there,
        "the agent is in the other project and no longer in this one"
    );
    // The machine is told, so what it keeps is filed under the new project too.
    r.frame(
        "the machine told",
        |f| matches!(f, HubFrame::Moved { project, .. } if project == "other"),
    )
    .await;
    // A bad project name, and a project this person does not own, are refused with a reason.
    r.event(
        "INTERACTION_CREATE",
        say("move", json!([{"name": "agent", "type": 3, "value": "nobody"}, {"name": "project", "type": 3, "value": "bad name!"}])),
    );
    r.until("the refusal", |l| {
        l.responses
            .iter()
            .find(|x| {
                x["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("letters, digits"))
            })
            .cloned()
    })
    .await;
}
