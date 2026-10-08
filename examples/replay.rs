//! A scripted multi-agent chat, replayed through three delivery policies, printed as JSON for the team cost benchmark.
//! The chat is one coding task: a lead splits it between two workers, they talk, a person adds instructions, they finish.
//! The only thing that differs between policies is what each agent is sent and when:
//!
//! - `naive_broadcast`  every message goes to every other agent, each as its own turn (what a plain group chat does)
//! - `addressed`        each message goes only to who it names (or the lead), each as its own turn
//! - `claudecord`       the real hub core: addressed, coalesced, with informing messages riding along
//!
//! The `claudecord` policy is produced by the real `HubCore`, so this measures the shipped behaviour, not a model of it.

use claudecord::agents::text::{Delivery, format_deliveries};
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use std::collections::BTreeMap;

/// One thing that happens in the chat.
enum Ev {
    /// A person writes to the room.
    Human(&'static str),
    /// An agent says something (a plain say, or addressed with @name).
    Say(&'static str, &'static str),
    /// The lead gives a task to a worker.
    Assign(&'static str, &'static str),
    /// A worker finishes a task.
    Done(&'static str, &'static str, &'static str),
}

const AGENTS: [(&str, &str, u64); 3] =
    [("otter", "mac", 1), ("heron", "gpu", 2), ("wren", "tpu", 3)];

fn script() -> Vec<Ev> {
    vec![
        Ev::Human(
            "Add a retry with exponential backoff to the HTTP client in src/net.rs, keep the public API unchanged, and cover it with tests.",
        ),
        Ev::Say(
            "otter",
            "Plan: heron implements the retry in net.rs, wren writes the tests.",
        ),
        Ev::Assign(
            "heron",
            "Implement retry with jitter in src/net.rs behind the existing send() function.",
        ),
        Ev::Assign(
            "wren",
            "Write tests for retry: success after two failures, give up after five, respects Retry-After.",
        ),
        Ev::Say("heron", "Using full jitter, base 200 ms, cap 10 s."),
        Ev::Say("wren", "I will mock the clock so the tests run fast."),
        Ev::Say(
            "heron",
            "@wren the function is retry_send(req, policy), tests can call that.",
        ),
        Ev::Say("wren", "@heron thanks, got it."),
        Ev::Human("@heron also log each retry at debug level"),
        Ev::Human("use the tracing crate for that"),
        Ev::Human("@heron no new dependencies please"),
        Ev::Say("heron", "Half done, wiring the logging now."),
        Ev::Done("heron", "T1", "retry added with jitter, 4 files changed"),
        Ev::Say("wren", "Tests written, 6 cases."),
        Ev::Done("wren", "T2", "6 tests, all passing"),
    ]
}

fn d(from: &str, text: &str) -> Delivery {
    Delivery {
        from: from.into(),
        text: text.into(),
        thread: None,
        msg_id: None,
    }
}

/// Who gets a message under the two simple policies. `broadcast` sends to everyone but the sender.
fn recipients(ev: &Ev, broadcast: bool) -> Vec<(&'static str, Delivery)> {
    let names: Vec<&str> = AGENTS.iter().map(|a| a.0).collect();
    let all_but =
        |s: &str| -> Vec<&'static str> { AGENTS.iter().map(|a| a.0).filter(|n| *n != s).collect() };
    let named = |text: &str| -> Vec<&'static str> {
        AGENTS
            .iter()
            .map(|a| a.0)
            .filter(|n| text.contains(&format!("@{n}")))
            .collect()
    };
    match ev {
        Ev::Human(t) => {
            let to = if broadcast {
                names
                    .iter()
                    .map(|n| AGENTS.iter().find(|a| a.0 == *n).unwrap().0)
                    .collect()
            } else {
                let n = named(t);
                if n.is_empty() { vec!["otter"] } else { n }
            };
            to.into_iter().map(|n| (n, d("kd (owner)", t))).collect()
        }
        Ev::Say(from, t) => {
            let to = if broadcast {
                all_but(from)
            } else {
                let n = named(t);
                if n.is_empty() && *from != "otter" {
                    vec!["otter"]
                } else {
                    n
                }
            };
            to.into_iter().map(|n| (n, d(from, t))).collect()
        }
        Ev::Assign(to, t) => vec![(AGENTS.iter().find(|a| a.0 == *to).unwrap().0, d("otter", t))],
        Ev::Done(from, id, s) => {
            let to = if broadcast {
                all_but(from)
            } else {
                vec!["otter"]
            };
            to.into_iter()
                .map(|n| (n, d("system", &format!("{from} done {id}: {s}"))))
                .collect()
        }
    }
}

/// The two simple policies: one turn per message per recipient.
fn simple(broadcast: bool) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> =
        AGENTS.iter().map(|a| (a.0.to_string(), vec![])).collect();
    for ev in script() {
        for (to, delivery) in recipients(&ev, broadcast) {
            out.get_mut(to)
                .unwrap()
                .push(format_deliveries(&[delivery]));
        }
    }
    out
}

/// The real hub core. Every effect that sends deliveries to one device in one call is one input for that agent.
fn claudecord() -> BTreeMap<String, Vec<String>> {
    let kd = Human {
        id: "1".into(),
        name: "kd".into(),
    };
    let mut core = HubCore::default();
    core.add_owner("1");
    let mut out: BTreeMap<String, Vec<String>> =
        AGENTS.iter().map(|a| (a.0.to_string(), vec![])).collect();
    let mut now = 1_000_000i64;
    let spec = |n: &str| AgentSpec {
        agent_id: format!("demo/{n}"),
        name: n.into(),
        project: "demo".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    };
    for (n, node, conn) in AGENTS {
        core.node_connected(node, conn);
        core.on_node_frame(
            node,
            NodeFrame::AgentRegister {
                agent: spec(n),
                cwd: "/x".into(),
            },
            now,
        );
    }
    let node_of = |n: &str| AGENTS.iter().find(|a| a.0 == n).unwrap().1;
    let record = |fx: Vec<Effect>, out: &mut BTreeMap<String, Vec<String>>| {
        let mut per: BTreeMap<u64, Vec<Delivery>> = BTreeMap::new();
        for e in fx {
            if let Effect::Send {
                conn,
                frame:
                    HubFrame::Deliver {
                        from,
                        text,
                        thread,
                        msg_id,
                        ..
                    },
            } = e
            {
                per.entry(conn).or_default().push(Delivery {
                    from,
                    text,
                    thread,
                    msg_id,
                });
            }
        }
        for (conn, items) in per {
            let name = AGENTS.iter().find(|a| a.2 == conn).unwrap().0;
            out.get_mut(name).unwrap().push(format_deliveries(&items));
        }
    };
    for ev in script() {
        now += 10_000;
        let fx = match ev {
            Ev::Human(t) => {
                core.human_message(&kd, "demo", t, &MessageOpts::default(), now)
                    .unwrap()
                    .1
            }
            Ev::Say(from, t) => core.on_node_frame(
                node_of(from),
                NodeFrame::AgentSay {
                    agent_id: format!("demo/{from}"),
                    text: t.into(),
                    thread: None,
                    say_id: None,
                },
                now,
            ),
            Ev::Assign(to, t) => core.on_node_frame(
                "mac",
                NodeFrame::AgentAssign {
                    agent_id: "demo/otter".into(),
                    to: to.into(),
                    task: t.into(),
                    thread: None,
                },
                now,
            ),
            Ev::Done(from, id, s) => core.on_node_frame(
                node_of(from),
                NodeFrame::AgentTaskDone {
                    agent_id: format!("demo/{from}"),
                    task_id: id.into(),
                    summary: s.into(),
                },
                now,
            ),
        };
        record(fx, &mut out);
    }
    // Anything still waiting to ride is delivered once it has waited long enough.
    record(core.tick(now + 3 * 60_000), &mut out);
    out
}

fn main() {
    let out = serde_json::json!({
        "naive_broadcast": simple(true),
        "addressed": simple(false),
        "claudecord": claudecord(),
        "agents": AGENTS.iter().map(|a| a.0).collect::<Vec<_>>(),
    });
    println!("{out}");
}
