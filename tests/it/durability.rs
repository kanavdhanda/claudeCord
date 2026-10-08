//! Durability: what is saved is exactly what changed, it is saved in one transaction with the history, and nothing is acknowledged
//! before it is on disk. The first test is the one that matters most: it drives the core with thousands of random operations and, all
//! the way, checks that rebuilding from only the rows that were saved gives exactly the state in memory. If any change were missed by
//! the tracking, a restart would quietly lose it, and this fails.

use claudecord::hub::{Answerer, Decision, HubCore, Human, MessageOpts, Role};
use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
use claudecord::server::{self, Config};
use claudecord::store::Store;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n.max(1)
    }
}

fn human(i: usize) -> Human {
    Human {
        id: format!("{}", 100 + i),
        name: format!("user{i}"),
    }
}

fn spec(project: &str, name: &str) -> AgentSpec {
    AgentSpec {
        agent_id: format!("{project}/{name}"),
        name: name.into(),
        project: project.into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    }
}

/// The state in memory and the state rebuilt from rows, as JSON values (so map order does not matter).
fn same_state(live: &HubCore, rows: Vec<(String, String)>) {
    let mut rebuilt = HubCore::default();
    rebuilt.restore_rows(rows);
    // Sets have no order, so their members are put in order before comparing.
    let sorted = |text: String| {
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for set in ["briefed", "roster_dirty", "owners"] {
            if let Some(list) = v[set].as_array_mut() {
                list.sort_by_key(|x| x.to_string());
            }
        }
        v
    };
    let (a, b) = (sorted(live.snapshot()), sorted(rebuilt.snapshot()));
    if a != b {
        // Say which part differs, and how, so a missed change is easy to find.
        let (am, bm) = (a.as_object().unwrap(), b.as_object().unwrap());
        for (k, av) in am {
            if bm.get(k) != Some(av) {
                let bs = bm.get(k).map(|v| v.to_string()).unwrap_or_default();
                panic!(
                    "the saved rows do not rebuild `{k}`:\n memory: {:.600}\n rebuilt: {:.600}",
                    av.to_string(),
                    bs
                );
            }
        }
    }
}

#[test]
fn saving_only_what_changed_always_rebuilds_exactly_the_state_in_memory() {
    let mut rng = Rng(42);
    let mut core = HubCore::default();
    core.add_owner("100");
    let mut db = Store::open_memory().unwrap();
    let projects = ["alpha", "beta", "gamma", "delta"];
    let names = ["otter", "heron", "finch", "lynx"];
    for n in 0..4 {
        core.node_connected(&format!("n{n}"), n as u64);
    }
    let mut now = 1_000i64;
    let owner = human(0);
    for step in 0..4_000 {
        now += rng.below(500) as i64;
        let (p, n) = (projects[rng.below(4)], names[rng.below(4)]);
        let node = format!("n{}", rng.below(4));
        match rng.below(14) {
            0 | 1 => {
                core.on_node_frame(
                    &node,
                    NodeFrame::AgentRegister {
                        agent: spec(p, n),
                        cwd: "/x".into(),
                    },
                    now,
                );
            }
            2 | 3 => {
                let _ = core.human_message(
                    &human(rng.below(3)),
                    p,
                    &format!("hello @{n} {step}"),
                    &MessageOpts::default(),
                    now,
                );
            }
            4 => {
                core.on_node_frame(
                    &node,
                    NodeFrame::AgentAsk {
                        agent_id: format!("{p}/{n}"),
                        ask_id: format!("a{step}"),
                        question: "which?".into(),
                        options: None,
                        thread: None,
                    },
                    now,
                );
            }
            5 => {
                let _ = core.answer_ask(&Answerer::Human(owner.clone()), p, "Q1", "this one", now);
            }
            6 => {
                core.on_node_frame(
                    &node,
                    NodeFrame::AgentPermission {
                        agent_id: format!("{p}/{n}"),
                        perm_id: format!("p{step}"),
                        kind: "bash".into(),
                        action: "ls".into(),
                        thread: None,
                    },
                    now,
                );
            }
            7 => {
                let d = [Decision::Once, Decision::Deny, Decision::Kind][rng.below(3)];
                let _ = core.decide_permission(&owner, p, "P1", d, None, now);
            }
            8 => {
                let _ = core.set_role(
                    &owner,
                    p,
                    &human(rng.below(4) + 1),
                    Some([Role::Operator, Role::Viewer][rng.below(2)]),
                );
            }
            9 => {
                core.on_node_frame(
                    &node,
                    NodeFrame::AgentSay {
                        agent_id: format!("{p}/{n}"),
                        text: format!("note {step} @{}", names[rng.below(4)]),
                        thread: None,
                        say_id: None,
                    },
                    now,
                );
            }
            10 => {
                core.node_disconnected(&node, rng.below(4) as u64);
                core.node_connected(&node, rng.below(4) as u64);
            }
            11 => {
                core.take_chat_message(&format!("c{}", rng.below(5)), rng.next() % 1000);
            }
            12 => {
                core.accept_seq(&node, 1 + rng.below(2) as u64, 1 + rng.below(50) as u64);
            }
            _ => {
                core.tick(now + 120_000);
            }
        }
        // Every few steps: save what changed, and check the rows rebuild the state.
        if step % 7 == 0 {
            let changes = core.take_changes();
            db.commit(&[], &[], &[], &[changes]).unwrap();
            same_state(&core, db.load_state().unwrap());
        }
    }
    let changes = core.take_changes();
    db.commit(&[], &[], &[], &[changes]).unwrap();
    same_state(&core, db.load_state().unwrap());
    assert!(
        db.load_state().unwrap().len() > 20,
        "the run made real state"
    );
}

#[test]
fn an_older_single_text_save_is_moved_into_rows_without_losing_anything() {
    let mut old = HubCore::default();
    old.add_owner("100");
    old.node_connected("n0", 1);
    old.on_node_frame(
        "n0",
        NodeFrame::AgentRegister {
            agent: spec("alpha", "otter"),
            cwd: "/x".into(),
        },
        10,
    );
    let text = old.snapshot();
    let mut moved = HubCore::default();
    assert!(moved.restore(&text));
    moved.mark_all_dirty();
    let mut db = Store::open_memory().unwrap();
    db.commit(&[], &[], &[], &[moved.take_changes()]).unwrap();
    same_state(&moved, db.load_state().unwrap());
    assert!(
        db.load_state()
            .unwrap()
            .iter()
            .any(|(k, _)| k == "agents:alpha/otter")
    );
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-dur-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn when_a_call_returns_its_change_is_already_on_disk_and_a_reader_sees_it() {
    let d = dir("ack");
    let mut core = HubCore::default();
    core.add_owner("100");
    let hub = server::start(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        },
        core,
        Store::open(&d.join("hub.db"), None).unwrap(),
    )
    .await
    .unwrap();
    hub.call(|c, now| {
        let fx = c.on_node_frame(
            "mac",
            NodeFrame::AgentRegister {
                agent: spec("demo", "otter"),
                cwd: "/x".into(),
            },
            now,
        );
        ((), fx)
    })
    .await;
    // A second connection to the same file, the way a restarted hub would read it.
    let reader = Store::open(&d.join("hub.db"), None).unwrap();
    for i in 0..60 {
        hub.call(move |c, now| {
            let fx = c
                .human_message(
                    &human(0),
                    "demo",
                    &format!("message {i}"),
                    &MessageOpts::default(),
                    now,
                )
                .map(|r| r.1)
                .unwrap_or_default();
            ((), fx)
        })
        .await;
        // The call has returned: the message is in the history on disk, and the agent's waiting mail is in the saved state.
        let seen = reader.history_latest("demo", None, 5).unwrap();
        assert!(
            seen.iter()
                .any(|r| r.text.contains(&format!("message {i}"))),
            "message {i} was acknowledged but is not on disk"
        );
        assert!(
            reader
                .load_state()
                .unwrap()
                .iter()
                .any(|(k, _)| k.starts_with("queues:demo/otter")),
            "the agent's waiting mail was not saved when the call returned"
        );
    }
    hub.shutdown().await;
}
