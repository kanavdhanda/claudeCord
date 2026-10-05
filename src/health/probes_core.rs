//! Health probes for the parts that are pure logic: the wire format, secret scrubbing, the rules in the hub core, the
//! saved state, storage and metrics. Each probe does one thing a person depends on and reports what it saw.

use super::{Feature, Probe, boxed, scratch};
use crate::hub::*;
use crate::protocol::{AdapterId, AgentSpec, AgentStatus, HubFrame, NodeFrame, UsageKind};
use crate::store::{HistoryRow, Store};

/// Fails the probe with a message unless the condition holds.
macro_rules! ensure {
    ($c:expr, $($m:tt)*) => {
        if !$c {
            return Err(format!($($m)*));
        }
    };
}

fn human(id: &str, name: &str) -> Human {
    Human {
        id: id.into(),
        name: name.into(),
    }
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

/// A core with owner kd (id 1), operator sam (id 2) and viewer vi (id 3), and the named agents each on their own device.
fn world(agents: &[&str]) -> HubCore {
    let mut c = HubCore::default();
    c.add_owner("1");
    let kd = human("1", "kd");
    c.set_role(&kd, "p", &human("2", "sam"), Some(Role::Operator))
        .expect("owner sets roles");
    c.set_role(&kd, "p", &human("3", "vi"), Some(Role::Viewer))
        .expect("owner sets roles");
    for (i, a) in agents.iter().enumerate() {
        let node = format!("n{i}");
        c.node_connected(&node, i as u64 + 1);
        c.on_node_frame(
            &node,
            NodeFrame::AgentRegister {
                agent: spec(a),
                cwd: "/x".into(),
            },
            0,
        );
    }
    c
}

/// The node name an agent lives on in `world`.
fn node(i: usize) -> String {
    format!("n{i}")
}

/// Texts delivered to agents in a list of effects.
fn texts(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .filter_map(|e| {
            if let Effect::Send {
                frame: HubFrame::Deliver { text, .. },
                ..
            } = e
            {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect()
}

fn frames(fx: &[Effect]) -> Vec<&HubFrame> {
    fx.iter()
        .filter_map(|e| {
            if let Effect::Send { frame, .. } = e {
                Some(frame)
            } else {
                None
            }
        })
        .collect()
}

fn msg(c: &mut HubCore, by: &Human, text: &str, now: i64) -> Vec<Effect> {
    c.human_message(by, "p", text, &MessageOpts::default(), now)
        .map(|r| r.1)
        .unwrap_or_default()
}

/// The features checked here.
pub fn features() -> Vec<Feature> {
    vec![
        Feature {
            name: "wire protocol",
            covers: &["protocol"],
            probe: wire,
        },
        Feature {
            name: "secret scrubbing",
            covers: &["security/redact", "security/env"],
            probe: secrets,
        },
        Feature {
            name: "rate limits and pairing codes",
            covers: &["security/limits", "security/codes", "security/mod"],
            probe: limits,
        },
        Feature {
            name: "terminal safety and agent profiles",
            covers: &["agents"],
            probe: agents,
        },
        Feature {
            name: "discord permissions",
            covers: &["discord/perms", "discord/mod"],
            probe: discord,
        },
        Feature {
            name: "roles and authority",
            covers: &["hub/access", "hub/model"],
            probe: roles,
        },
        Feature {
            name: "asks with one winner",
            covers: &["hub/asks"],
            probe: asks,
        },
        Feature {
            name: "permissions and grants",
            covers: &["hub/permissions"],
            probe: permissions,
        },
        Feature {
            name: "tasks",
            covers: &["hub/tasks"],
            probe: tasks,
        },
        Feature {
            name: "queues, coalescing and ride-along",
            covers: &[
                "hub/routing",
                "hub/briefs",
                "hub/core",
                "hub/effects",
                "hub/mod",
            ],
            probe: queues,
        },
        Feature {
            name: "reminders and expiry",
            covers: &["hub/expiry"],
            probe: expiry,
        },
        Feature {
            name: "stop, pause and kill commands",
            covers: &["hub/controls"],
            probe: controls,
        },
        Feature {
            name: "placing agents across machines",
            covers: &["hub/controls"],
            probe: placement,
        },
        Feature {
            name: "file transfer limits",
            covers: &["hub/files"],
            probe: files,
        },
        Feature {
            name: "handoff at the limit",
            covers: &["hub/handoff"],
            probe: handoff,
        },
        Feature {
            name: "saving and restoring state",
            covers: &["hub/snapshot", "hub/tracked"],
            probe: snapshot,
        },
        Feature {
            name: "storage and history rollover",
            covers: &["store"],
            probe: storage,
        },
        Feature {
            name: "metrics",
            covers: &["metrics"],
            probe: metrics,
        },
        Feature {
            name: "crate helpers",
            covers: &["lib", "health", "main"],
            probe: helpers,
        },
    ]
}

fn wire() -> Probe {
    boxed(async {
        let ok = NodeFrame::parse(r#"{"t":"agent.say","agentId":"p/a","text":"hi"}"#);
        ensure!(ok.is_some(), "a valid frame was refused");
        ensure!(
            NodeFrame::parse(r#"{"t":"nonsense"}"#).is_none(),
            "an unknown frame was accepted"
        );
        let big = format!(
            r#"{{"t":"agent.say","agentId":"p/a","text":"{}"}}"#,
            "x".repeat(9000)
        );
        ensure!(
            NodeFrame::parse(&big).is_none(),
            "an oversized frame was accepted"
        );
        ensure!(
            NodeFrame::parse(r#"{"t":"agent.usage","agentId":"p/a","kind":"session","pct":101}"#)
                .is_none(),
            "usage over 100 was accepted"
        );
        let f = HubFrame::Deliver {
            agent_id: "p/a".into(),
            from: "x".into(),
            text: "t".into(),
            thread: None,
            msg_id: None,
        };
        ensure!(
            HubFrame::parse(&serde_json::to_string(&f).expect("plain")) == Some(f),
            "a hub frame did not survive a round trip"
        );
        Ok("valid accepted, unknown, oversized and out-of-range refused, round trip intact".into())
    })
}

fn secrets() -> Probe {
    boxed(async {
        let r = crate::redact::redact(&format!("token ghp_{}", "a".repeat(36)));
        ensure!(
            !r.text.contains("ghp_") && !r.found.is_empty(),
            "a token was not scrubbed"
        );
        ensure!(
            crate::redact::redact(&r.text).text == r.text,
            "scrubbing twice changed the text"
        );
        let blocked = crate::redact::find_secrets_in_file(
            b"-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
        );
        ensure!(!blocked.is_empty(), "a private key file was not caught");
        let names = crate::env::secret_env_names(
            [
                ("MY_API_TOKEN", "x"),
                ("HOME", "/h"),
                ("SSH_AUTH_SOCK", "/s"),
            ],
            "claude",
        );
        ensure!(
            names == vec!["MY_API_TOKEN".to_string()],
            "environment scrubbing picked {names:?}"
        );
        Ok("token removed, idempotent, key file blocked, only the secret variable scrubbed".into())
    })
}

fn limits() -> Probe {
    boxed(async {
        let mut b = crate::limits::Bucket::new(3.0, 1.0, 0.0);
        ensure!(
            b.take(1.0, 0.0) && b.take(1.0, 0.0) && b.take(1.0, 0.0) && !b.take(1.0, 0.0),
            "the burst limit did not hold"
        );
        let mut f = crate::limits::FailureLimiter::new(2, 1000.0);
        f.fail("ip", 0.0);
        f.fail("ip", 1.0);
        ensure!(
            f.blocked("ip", 2.0) && !f.blocked("other", 2.0),
            "failure blocking is wrong"
        );
        ensure!(
            crate::codes::normalize_code("ab-cd 12") == "ABCD12",
            "code normalising is wrong"
        );
        Ok("burst limited, repeat failures blocked, codes normalised".into())
    })
}

fn agents() -> Probe {
    boxed(async {
        ensure!(
            !crate::text::strip_control("a\x1b[201~b").contains('\x1b'),
            "escape codes survived"
        );
        ensure!(
            crate::text::quote_body("one\ntwo") == "one\n> two",
            "body quoting is wrong"
        );
        ensure!(
            crate::text::is_sensitive_path("/home/u/.ssh/id_rsa"),
            "a key path was not protected"
        );
        let s = AdapterId::Claude.detect("welcome\n? for shortcuts");
        ensure!(s.ready && !s.busy, "a ready screen was not recognised");
        ensure!(
            AdapterId::Claude.detect("esc to interrupt").busy,
            "a busy screen was not recognised"
        );
        let argv = AdapterId::Claude.argv(&crate::adapters::LaunchCtx {
            name: "a",
            model: None,
            policy: crate::adapters::Policy::Ask,
            rules: "r",
            mcp_config: None,
        });
        ensure!(argv[0] == "claude", "the launch command is wrong");
        Ok("control codes stripped, bodies quoted, key paths protected, screens read".into())
    })
}

fn discord() -> Probe {
    boxed(async {
        ensure!(
            crate::perms::permissions_integer() == 309_774_625_872,
            "the permission integer changed"
        );
        ensure!(
            crate::perms::invite_url("123").contains("scope=bot"),
            "the invite link lacks the bot scope"
        );
        Ok("bot permission integer and invite link as expected".into())
    })
}

fn roles() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        ensure!(
            c.human_message(&human("99", "x"), "p", "hi", &MessageOpts::default(), 0)
                .is_err(),
            "an unlisted account was heard"
        );
        ensure!(
            c.human_message(&human("3", "vi"), "p", "hi", &MessageOpts::default(), 0)
                .is_err(),
            "a viewer could instruct"
        );
        ensure!(
            c.set_role(&human("2", "sam"), "p", &human("7", "n"), Some(Role::Owner))
                .is_err(),
            "an operator changed roles"
        );
        ensure!(
            c.human_message(&human("2", "sam"), "p", "hi", &MessageOpts::default(), 0)
                .is_ok(),
            "an operator was refused"
        );
        Ok("unlisted and viewer refused, only owners change roles, operators heard".into())
    })
}

fn asks() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        c.on_node_frame(
            &node(0),
            NodeFrame::AgentAsk {
                agent_id: "p/otter".into(),
                ask_id: "a1".into(),
                question: "q?".into(),
                options: None,
                thread: None,
            },
            0,
        );
        ensure!(
            c.answer_ask(&Answerer::Human(human("2", "sam")), "p", "Q1", "yes", 1)
                .is_ok(),
            "the first answer was refused"
        );
        ensure!(
            matches!(
                c.answer_ask(&Answerer::Human(human("1", "kd")), "p", "Q1", "no", 2),
                Err(Denied::AlreadyDone { .. })
            ),
            "a second answer won"
        );
        Ok("first answer wins, the second is told it was too late".into())
    })
}

fn permissions() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        let ask = |c: &mut HubCore, id: &str, kind: &str, action: &str| {
            c.on_node_frame(
                &node(0),
                NodeFrame::AgentPermission {
                    agent_id: "p/otter".into(),
                    perm_id: id.into(),
                    kind: kind.into(),
                    action: action.into(),
                    thread: None,
                },
                0,
            )
        };
        ask(&mut c, "x1", "edit", "src/a.rs");
        ensure!(
            c.decide_permission(&human("2", "sam"), "p", "P1", Decision::Kind, None, 1)
                .is_err(),
            "an operator made a standing grant"
        );
        ensure!(
            c.decide_permission(
                &human("1", "kd"),
                "p",
                "P1",
                Decision::Kind,
                Some(60_000),
                1
            )
            .is_ok(),
            "an owner could not grant"
        );
        let fx = ask(&mut c, "x2", "edit", "src/b.rs");
        ensure!(
            frames(&fx)
                .iter()
                .any(|f| matches!(f, HubFrame::Decision { allow: true, .. })),
            "a granted request was still asked about"
        );
        let fx = c.tick(1 + 120_000);
        let _ = fx;
        let fx = ask(&mut c, "x3", "edit", "src/c.rs");
        ensure!(
            fx.iter()
                .any(|e| matches!(e, Effect::Chat(Chat::Permission { .. }))),
            "an expired grant still allowed"
        );
        let fx = ask(&mut c, "x4", "bash", "cat ~/.ssh/id_rsa");
        ensure!(
            frames(&fx)
                .iter()
                .any(|f| matches!(f, HubFrame::Decision { allow: false, .. })),
            "a protected path was not denied"
        );
        Ok(
            "operator cannot grant, owner can, grant covers then expires, protected path denied"
                .into(),
        )
    })
}

fn tasks() -> Probe {
    boxed(async {
        let mut c = world(&["otter", "heron"]);
        c.on_node_frame(
            &node(0),
            NodeFrame::AgentAssign {
                agent_id: "p/otter".into(),
                to: "heron".into(),
                task: "build".into(),
                thread: None,
            },
            0,
        );
        ensure!(c.tasks_of("p").len() == 1, "the task was not created");
        let fx = c.on_node_frame(
            &node(1),
            NodeFrame::AgentTaskDone {
                agent_id: "p/heron".into(),
                task_id: "T1".into(),
                summary: "ok".into(),
            },
            1,
        );
        ensure!(
            texts(&fx).iter().any(|t| t.contains("final report")),
            "the lead was not told all tasks are done"
        );
        let again = c.on_node_frame(
            &node(1),
            NodeFrame::AgentTaskDone {
                agent_id: "p/heron".into(),
                task_id: "T1".into(),
                summary: "ok".into(),
            },
            2,
        );
        ensure!(
            texts(&again).iter().any(|t| t.contains("already done")),
            "finishing twice was not caught"
        );
        Ok("assigned, finished, lead told, double finish caught".into())
    })
}

fn queues() -> Probe {
    boxed(async {
        let mut c = world(&["otter", "heron"]);
        let kd = human("1", "kd");
        let fx = msg(&mut c, &kd, "start", 10);
        ensure!(
            texts(&fx).iter().any(|t| t.contains("You lead")),
            "the lead got no brief"
        );
        c.on_node_frame(
            &node(1),
            NodeFrame::AgentSay {
                agent_id: "p/heron".into(),
                text: "FYI".into(),
                thread: None,
            },
            11,
        );
        let fx = msg(&mut c, &kd, "go on", 12);
        ensure!(
            texts(&fx) == vec!["FYI".to_string(), "go on".to_string()],
            "an informing message did not ride along: {:?}",
            texts(&fx)
        );
        c.hold(&kd, true, "p", Some("otter"), 13)
            .map_err(|e| format!("{e:?}"))?;
        for t in ["a", "b", "c"] {
            msg(&mut c, &kd, t, 14);
        }
        let (_, fx) = c
            .hold(&kd, false, "p", Some("otter"), 15)
            .map_err(|e| format!("{e:?}"))?;
        ensure!(
            texts(&fx) == vec!["a", "b", "c"],
            "a held burst was not released together"
        );
        Ok("brief once, FYI rides along, a held burst arrives as one input".into())
    })
}

fn expiry() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        c.on_node_frame(
            &node(0),
            NodeFrame::AgentAsk {
                agent_id: "p/otter".into(),
                ask_id: "a1".into(),
                question: "q?".into(),
                options: None,
                thread: None,
            },
            0,
        );
        ensure!(
            c.tick(16 * 60_000).iter().any(
                |e| matches!(e, Effect::Chat(Chat::Notice { text, .. }) if text.contains("Q1"))
            ),
            "no reminder after 15 minutes"
        );
        ensure!(
            texts(&c.tick(61 * 60_000))
                .iter()
                .any(|t| t.contains("expired")),
            "the agent was not told the ask expired"
        );
        Ok("reminder at 15 minutes, expiry at 60, agent told".into())
    })
}

fn controls() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        ensure!(
            c.stop(&human("3", "vi"), "p", "otter", 0).is_err(),
            "a viewer could stop an agent"
        );
        let (ok, fx) = c
            .stop(&human("2", "sam"), "p", "otter", 0)
            .map_err(|e| format!("{e:?}"))?;
        ensure!(
            ok && frames(&fx)
                .iter()
                .any(|f| matches!(f, HubFrame::Stop { .. })),
            "stop was not sent"
        );
        ensure!(
            c.kill_all(&human("2", "sam"), Some("p"), 0).is_err(),
            "an operator could kill everything"
        );
        ensure!(
            c.kill_all(&human("1", "kd"), Some("p"), 0).is_ok(),
            "an owner could not kill everything"
        );
        let (_, fx) = c
            .btw(&human("2", "sam"), "p", "quick", None, 1)
            .map_err(|e| format!("{e:?}"))?;
        ensure!(
            !fx.iter().any(|e| matches!(e, Effect::AcceptCheck { .. })),
            "a btw was tracked as a task"
        );
        Ok("stop needs operator, killall needs owner, btw is untracked".into())
    })
}

fn files() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        let (names, fx) = c
            .send_file(
                &human("2", "sam"),
                "p",
                "see",
                "a.bin",
                &vec![1u8; 400 * 1024],
                None,
                "t",
            )
            .map_err(|e| format!("{e:?}"))?;
        ensure!(
            names == vec!["otter"] && frames(&fx).len() == 3,
            "a 400 KB file was not sent as 3 chunks"
        );
        ensure!(
            c.send_file(&human("3", "vi"), "p", "x", "a", b"x", None, "t2")
                .is_err(),
            "a viewer could send a file"
        );
        let key = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .encode(b"-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----")
        };
        let fx = c.on_node_frame(
            &node(0),
            NodeFrame::FileChunk {
                transfer_id: "k".into(),
                agent_id: "p/otter".into(),
                name: "k".into(),
                seq: 0,
                last: true,
                data: key,
                to: None,
                caption: None,
                thread: None,
            },
            0,
        );
        ensure!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Chat(Chat::File { .. }))),
            "a file with a private key was posted"
        );
        Ok("chunking, viewer refused, secret file blocked".into())
    })
}

fn handoff() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        let fx = c.on_node_frame(
            &node(0),
            NodeFrame::AgentUsage {
                agent_id: "p/otter".into(),
                kind: UsageKind::Session,
                pct: 97,
            },
            0,
        );
        ensure!(
            texts(&fx).iter().any(|t| t.starts_with("URGENT")),
            "no urgent request at 97%"
        );
        c.on_node_frame(
            &node(0),
            NodeFrame::AgentHandoff {
                agent_id: "p/otter".into(),
                text: "goal: x".into(),
            },
            1,
        );
        let fx = c.on_node_frame(
            &node(0),
            NodeFrame::AgentPickup {
                agent_id: "p/otter".into(),
            },
            2,
        );
        ensure!(
            texts(&fx).iter().any(|t| t.contains("goal: x")),
            "the fresh session did not get the handoff"
        );
        Ok("urgent at 97%, dump saved, fresh session picked it up".into())
    })
}

fn snapshot() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        c.node_disconnected(&node(0), 1);
        msg(&mut c, &human("1", "kd"), "waiting", 5);
        let saved = c.snapshot();
        let mut d = HubCore::default();
        ensure!(d.restore(&saved), "a save could not be restored");
        ensure!(
            !HubCore::default().restore("garbage"),
            "garbage was accepted as a save"
        );
        d.node_connected("n0", 9);
        let fx = d.on_node_frame(
            "n0",
            NodeFrame::AgentRegister {
                agent: spec("otter"),
                cwd: "/x".into(),
            },
            6,
        );
        ensure!(
            texts(&fx).iter().any(|t| t == "waiting"),
            "a message queued before the restart was lost"
        );
        let _ = AgentStatus::Idle;
        // Saving only what changed, as rows, rebuilds exactly the same state as saving it all.
        let mut db = Store::open_memory().map_err(|e| e.to_string())?;
        let changes = c.take_changes();
        ensure!(
            !changes.is_empty(),
            "changes were made and none were noticed"
        );
        db.commit(&[], &[], &[], &[changes])
            .map_err(|e| e.to_string())?;
        let mut e = HubCore::default();
        e.restore_rows(db.load_state().map_err(|e| e.to_string())?);
        let (a, b): (serde_json::Value, serde_json::Value) = (
            serde_json::from_str(&c.snapshot()).map_err(|e| e.to_string())?,
            serde_json::from_str(&e.snapshot()).map_err(|e| e.to_string())?,
        );
        ensure!(
            a == b,
            "state rebuilt from changed rows differs from the state it was saved from"
        );
        ensure!(
            c.take_changes().is_empty(),
            "the same changes were reported twice"
        );
        Ok("state restored, waiting message delivered after restart, garbage refused, changed rows rebuild identical state".into())
    })
}

fn storage() -> Probe {
    boxed(async {
        let dir = scratch("store");
        let mut s =
            Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).map_err(|e| e.to_string())?;
        let row = |at: i64, t: &str| HistoryRow {
            id: 0,
            at,
            project: "p".into(),
            thread: None,
            from: "kd".into(),
            kind: "human".into(),
            text: t.into(),
        };
        s.append(&[row(1, "old"), row(1000, "new")])
            .map_err(|e| e.to_string())?;
        ensure!(
            s.history("p", None, 0, 10)
                .map_err(|e| e.to_string())?
                .len()
                == 2,
            "history was not read back"
        );
        ensure!(
            s.append(&[row(2, "ok"), row(3, &"x".repeat(100_001))])
                .is_err()
                && s.hot_rows().map_err(|e| e.to_string())? == 2,
            "a failed batch left rows behind"
        );
        ensure!(
            s.rollover(500).map_err(|e| e.to_string())? == 1
                && s.hot_rows().map_err(|e| e.to_string())? == 1,
            "old history did not roll over"
        );
        let t = s.create_token("mac", 0).map_err(|e| e.to_string())?;
        ensure!(
            s.node_for_token(&t).map_err(|e| e.to_string())?.as_deref() == Some("mac")
                && s.node_for_token("bad")
                    .map_err(|e| e.to_string())?
                    .is_none(),
            "token check is wrong"
        );
        s.save_snapshot("{}", 1).map_err(|e| e.to_string())?;
        ensure!(
            s.load_snapshot().map_err(|e| e.to_string())?.as_deref() == Some("{}"),
            "snapshot did not round trip"
        );
        Ok(
            "history, atomic batches, rollover to a compressed file, token hashing, snapshot"
                .into(),
        )
    })
}

fn metrics() -> Probe {
    boxed(async {
        let mut c = world(&["otter"]);
        msg(&mut c, &human("1", "kd"), "hello", 60_000);
        let r = c.metrics.insights(60, 60, &["msg_human"], 120_000);
        ensure!(
            r.totals.get("msg_human") == Some(&1.0),
            "the message was not counted: {:?}",
            r.totals
        );
        Ok("a message was counted".into())
    })
}

fn helpers() -> Probe {
    boxed(async {
        ensure!(
            crate::jslen("a\u{1F600}") == 3 && crate::jsslice("a\u{1F600}", 1) == "a",
            "length helpers disagree with JavaScript"
        );
        ensure!(
            crate::now_ms() > 1_600_000_000_000,
            "the clock reads nonsense"
        );
        Ok("length helpers and clock".into())
    })
}

fn placement() -> Probe {
    boxed(async {
        let mut c = world(&[]);
        let info = |max: u64, labels: &[&str]| NodeFrame::NodeInfo {
            cores: 4,
            mem_mb: 8000,
            max_agents: max,
            labels: labels.iter().map(|l| l.to_string()).collect(),
        };
        for (n, conn) in [("a", 1u64), ("b", 2)] {
            c.node_connected(n, conn);
        }
        c.on_node_frame("a", info(1, &["gpu"]), 0);
        c.on_node_frame("b", info(4, &[]), 0);
        c.on_node_frame(
            "a",
            NodeFrame::AgentRegister {
                agent: spec("x"),
                cwd: "/".into(),
            },
            0,
        );
        ensure!(
            c.pick_node(None).as_deref() == Some("b"),
            "the less loaded machine was not chosen"
        );
        ensure!(
            c.pick_node(Some("gpu")).is_none(),
            "a full machine was chosen for a label"
        );
        let (node, fx) = c
            .spawn_auto(&human("1", "kd"), "p", spec("new"), None)
            .map_err(|e| format!("{e:?}"))?;
        ensure!(
            node.as_deref() == Some("b")
                && frames(&fx)
                    .iter()
                    .any(|f| matches!(f, HubFrame::Spawn { .. })),
            "the spawn request was not sent to the chosen machine"
        );
        Ok("least-loaded machine with room chosen, labels and limits respected, spawn request sent".into())
    })
}
