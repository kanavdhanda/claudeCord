//! Races: many things happening at the same moment. Each test starts a real hub and fires the competing actions together from
//! many tasks or threads, then checks the rule that must hold whatever the order: one winner, nothing lost, nothing doubled,
//! nothing panics, and the hub still answers afterwards.

use claudecord::hub::{Answerer, Decision, Denied, HubCore, Human, MessageOpts, Role};
use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
use claudecord::server::{self, Config};
use claudecord::store::Store;

const N: usize = 64;

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-race-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn human(i: usize) -> Human {
    Human {
        id: format!("{}", 1000 + i),
        name: format!("user{i}"),
    }
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

/// A hub with owner "1", N operators, and agent `otter` registered from machine `mac`.
async fn rig(name: &str) -> (server::Hub, std::path::PathBuf) {
    let d = dir(name);
    let mut core = HubCore::default();
    core.add_owner("1");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..Config::default()
    };
    let hub = server::start(cfg, core, Store::open(&d.join("hub.db"), None).unwrap())
        .await
        .unwrap();
    hub.call(|c, now| {
        let owner = Human {
            id: "1".into(),
            name: "kd".into(),
        };
        let mut fx = c.on_node_frame(
            "mac",
            NodeFrame::AgentRegister {
                agent: spec("otter"),
                cwd: "/x".into(),
            },
            now,
        );
        for i in 0..N {
            c.set_role(&owner, "demo", &human(i), Some(Role::Operator))
                .unwrap();
        }
        fx.extend(vec![]);
        ((), fx)
    })
    .await;
    (hub, d)
}

#[tokio::test]
async fn when_many_people_answer_one_question_at_once_exactly_one_wins() {
    let (hub, _) = rig("ask").await;
    hub.call(|c, now| {
        let fx = c.on_node_frame(
            "mac",
            NodeFrame::AgentAsk {
                agent_id: "demo/otter".into(),
                ask_id: "a1".into(),
                question: "csv or json?".into(),
                options: None,
                thread: None,
            },
            now,
        );
        ((), fx)
    })
    .await;
    let handle = hub.handle();
    let tasks: Vec<_> = (0..N)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .call(move |c, now| {
                        let r = c.answer_ask(
                            &Answerer::Human(human(i)),
                            "demo",
                            "Q1",
                            &format!("answer {i}"),
                            now,
                        );
                        (r.is_ok(), vec![])
                    })
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1, "one answer is accepted, the rest are told who won");
    hub.shutdown().await;
}

#[tokio::test]
async fn when_many_people_decide_one_permission_at_once_exactly_one_decision_counts() {
    let (hub, _) = rig("perm").await;
    hub.call(|c, now| {
        let fx = c.on_node_frame(
            "mac",
            NodeFrame::AgentPermission {
                agent_id: "demo/otter".into(),
                perm_id: "p1".into(),
                kind: "bash".into(),
                action: "ls".into(),
                thread: None,
            },
            now,
        );
        ((), fx)
    })
    .await;
    let handle = hub.handle();
    let tasks: Vec<_> = (0..N)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .call(move |c, now| {
                        let decision = if i % 2 == 0 {
                            Decision::Once
                        } else {
                            Decision::Deny
                        };
                        let r = c.decide_permission(&human(i), "demo", "P1", decision, None, now);
                        (r.is_ok(), vec![])
                    })
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1, "a request is decided once, whoever is fastest");
    let again = hub
        .call(|c, now| {
            (
                c.decide_permission(&human(0), "demo", "P1", Decision::Once, None, now)
                    .err(),
                vec![],
            )
        })
        .await
        .unwrap();
    assert!(matches!(again, Some(Denied::AlreadyDone { .. })));
    hub.shutdown().await;
}

#[tokio::test]
async fn messages_sent_at_the_same_time_are_all_kept_once_and_in_one_order() {
    let (hub, d) = rig("flood").await;
    let handle = hub.handle();
    let tasks: Vec<_> = (0..N * 4)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .call(move |c, now| {
                        let fx = c
                            .human_message(
                                &human(i % N),
                                "demo",
                                &format!("msg {i}"),
                                &MessageOpts::default(),
                                now,
                            )
                            .unwrap()
                            .1;
                        ((), fx)
                    })
                    .await
                    .unwrap();
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    hub.shutdown().await;
    let db = Store::open(&d.join("hub.db"), None).unwrap();
    let rows = db.history_latest("demo", None, 500).unwrap();
    let mut texts: Vec<_> = rows
        .iter()
        .filter(|r| r.text.starts_with("msg "))
        .map(|r| r.text.clone())
        .collect();
    texts.sort();
    texts.dedup();
    assert_eq!(
        texts.len(),
        N * 4,
        "every message was recorded exactly once"
    );
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert!(
        sorted.windows(2).all(|w| w[0] < w[1]),
        "row ids are unique and ordered"
    );
}

#[tokio::test]
async fn the_same_agent_registered_from_many_machines_at_once_is_one_agent_with_one_owner() {
    let (hub, _) = rig("register").await;
    let handle = hub.handle();
    let tasks: Vec<_> = (0..N)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .call(move |c, now| {
                        let fx = c.on_node_frame(
                            &format!("box{i}"),
                            NodeFrame::AgentRegister {
                                agent: spec("heron"),
                                cwd: "/x".into(),
                            },
                            now,
                        );
                        ((), fx)
                    })
                    .await
                    .unwrap();
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let (count, answers) = hub
        .call(|c, _| {
            (
                (
                    c.agents_of_project("demo")
                        .iter()
                        .filter(|a| a.name == "heron")
                        .count(),
                    c.agents_of_project("demo").len(),
                ),
                vec![],
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 1, "never two heron");
    assert_eq!(answers, 2, "otter and heron");
    hub.shutdown().await;
}

#[test]
fn two_writers_to_the_uptime_log_at_once_never_corrupt_it_or_fail() {
    let d = dir("uptime");
    let path = d.join("hub.db");
    drop(Store::open(&path, None).unwrap());
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let path = path.clone();
            std::thread::spawn(move || {
                let db = Store::open(&path, None).unwrap();
                // Few enough synced commits that a slow disk never makes one writer wait out the lock timeout.
                for i in 0..8 {
                    let state = if (i + t) % 2 == 0 {
                        claudecord::uptime::State::Up
                    } else {
                        claudecord::uptime::State::Down
                    };
                    db.uptime_set(&format!("c{}", t % 2), state, (t * 1000 + i) as i64)
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let db = Store::open(&path, None).unwrap();
    assert_eq!(db.uptime_components().unwrap(), ["c0", "c1"]);
    for c in ["c0", "c1"] {
        let changes = db.uptime_changes(c, 0).unwrap();
        assert!(!changes.is_empty());
    }
}
