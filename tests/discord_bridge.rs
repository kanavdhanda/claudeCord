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
    let (fake, addr) = start_fake().await;
    let dir = tmp(name);
    let db = dir.join("hub.db");
    let mut store = Store::open(&db, None).unwrap();
    let token = store.create_token("mac", 0).unwrap();
    let hub = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ping_every: Duration::from_millis(300),
            save_every: Duration::from_millis(20),
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
        api_base: format!("http://{addr}/api"),
        token: "bottoken".into(),
        guild: "g1".into(),
        gateway_url: None,
        db_path: db,
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
            l.reactions.iter().find(|x| x["message"] == "m1").cloned()
        })
        .await;
    assert_eq!(reaction["channel"], ch.as_str());
    assert_eq!(reaction["emoji"], "\u{2705}", "{reaction}");
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
    r.event("MESSAGE_CREATE", message(&ch, "m7", "1", "after the drop"));
    r.frame("a message after reconnecting", |f| {
        deliver_text(f, "after the drop")
    })
    .await;
}
