//! Token budget, measured on a scripted chat. The cost of coordination that lands in an agent's context is what this
//! checks: how many inputs each agent receives (every input is a turn that rereads the whole context), and how many
//! tokens of framing and instructions ride along with the actual message text.
//!
//! Tokens are estimated as characters / 4, rounded up, which is a deliberately rough upper-ish bound for English. The
//! test prints a report (run with `--nocapture`) and fails when a budget from learnings/08-token-budget.md is broken.

use claudecord::agents::text::{Delivery, format_deliveries};
use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, AgentStatus, HubFrame, NodeFrame};
use std::collections::BTreeMap;

const T0: i64 = 1_000_000;

fn tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
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

/// What the agents received, per agent: inputs (turns) and the formatted text of each.
#[derive(Default)]
struct Ledger {
    inputs: BTreeMap<u64, Vec<Vec<Delivery>>>,
    body_tokens: usize,
    framed_tokens: usize,
    max_frame_overhead: usize,
    max_preface: usize,
}

impl Ledger {
    /// Records the deliveries inside one batch of effects. Deliveries to the same device in one batch are one input.
    fn record(&mut self, fx: &[Effect]) {
        let mut per_conn: BTreeMap<u64, Vec<Delivery>> = BTreeMap::new();
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
                per_conn.entry(*conn).or_default().push(Delivery {
                    from: from.clone(),
                    text: text.clone(),
                    thread: thread.clone(),
                    msg_id: msg_id.clone(),
                });
            }
        }
        for (conn, items) in per_conn {
            for d in &items {
                let framed = tokens(&format_deliveries(std::slice::from_ref(d)));
                if d.from == "system" {
                    self.max_preface = self.max_preface.max(tokens(&d.text));
                    self.framed_tokens += framed;
                } else {
                    self.body_tokens += tokens(&d.text);
                    self.framed_tokens += framed;
                    self.max_frame_overhead = self
                        .max_frame_overhead
                        .max(framed.saturating_sub(tokens(&d.text)));
                }
            }
            self.inputs.entry(conn).or_default().push(items);
        }
    }
}

#[test]
fn coordination_overhead_stays_within_budget_on_a_sample_chat() {
    let mut core = HubCore::default();
    let mut led = Ledger::default();
    let kd = human("1", "kd");
    core.add_owner("1");
    for (node, conn, name) in [
        ("mac", 1u64, "otter"),
        ("gpu", 2, "heron"),
        ("tpu", 3, "wren"),
    ] {
        core.node_connected(node, conn);
        led.record(&core.on_node_frame(
            node,
            NodeFrame::AgentRegister {
                agent: spec(name),
                cwd: "/x".into(),
            },
            T0,
        ));
    }
    let say = |core: &mut HubCore, led: &mut Ledger, node: &str, who: &str, text: &str| {
        led.record(&core.on_node_frame(
            node,
            NodeFrame::AgentSay {
                agent_id: format!("p/{who}"),
                text: text.into(),
                thread: Some("T1".into()),
            },
            T0,
        ))
    };

    // The person gives the lead a task.
    let (_, fx) = core.human_message(&kd, "p", "Add a retry with exponential backoff to the HTTP client in src/net.rs, keep the public API unchanged, and cover it with tests.", &MessageOpts { reference: Some("c:1"), ..Default::default() }, T0).unwrap();
    led.record(&fx);
    // The lead splits it.
    for (to, task) in [
        (
            "heron",
            "Implement retry with jitter in src/net.rs behind the existing send() function.",
        ),
        (
            "wren",
            "Write tests for retry: success after two failures, give up after five, respects Retry-After.",
        ),
    ] {
        led.record(&core.on_node_frame(
            "mac",
            NodeFrame::AgentAssign {
                agent_id: "p/otter".into(),
                to: to.into(),
                task: task.into(),
                thread: Some("T1".into()),
            },
            T0,
        ));
    }
    // Workers chat a little with the lead, unaddressed, so it goes to the lead only.
    say(
        &mut core,
        &mut led,
        "gpu",
        "heron",
        "Using full jitter, base 200 ms, cap 10 s. Okay with you?",
    );
    say(&mut core, &mut led, "mac", "otter", "@heron yes, go ahead.");
    // A permission request costs the agent no tokens: decided by the person, outside the model.
    led.record(&core.on_node_frame(
        "gpu",
        NodeFrame::AgentPermission {
            agent_id: "p/heron".into(),
            perm_id: "x1".into(),
            kind: "bash".into(),
            action: "cargo test -p net".into(),
            thread: None,
        },
        T0,
    ));
    led.record(
        &core
            .decide_permission(&kd, "p", "P1", Decision::Kind, None, T0 + 1)
            .unwrap(),
    );
    // The person sends an aside while the workers run.
    let (_, fx) = core
        .btw(
            &kd,
            "p",
            "heads up: CI is slow today",
            Some("heron"),
            T0 + 2,
        )
        .unwrap();
    led.record(&fx);
    // Five messages for one agent arrive while it is paused. Resuming must release them as ONE input.
    core.hold(&kd, true, "p", Some("heron"), T0 + 3).unwrap();
    for t in [
        "@heron also log each retry at debug level",
        "@heron use the tracing crate",
        "@heron no new dependencies please",
        "@heron keep functions short",
        "@heron thanks",
    ] {
        let (_, fx) = core
            .human_message(&kd, "p", t, &MessageOpts::default(), T0 + 4)
            .unwrap();
        led.record(&fx);
    }
    let (_, fx) = core.hold(&kd, false, "p", Some("heron"), T0 + 5).unwrap();
    let mut burst = Ledger::default();
    burst.record(&fx);
    assert_eq!(
        burst.inputs.get(&2).map_or(0, Vec::len),
        1,
        "five held messages are released as ONE input"
    );
    led.record(&fx);
    // Workers finish.
    led.record(&core.on_node_frame(
        "gpu",
        NodeFrame::AgentTaskDone {
            agent_id: "p/heron".into(),
            task_id: "T1".into(),
            summary: "retry added with jitter, 4 files changed".into(),
        },
        T0 + 9,
    ));
    led.record(&core.on_node_frame(
        "tpu",
        NodeFrame::AgentTaskDone {
            agent_id: "p/wren".into(),
            task_id: "T2".into(),
            summary: "6 tests, all passing".into(),
        },
        T0 + 10,
    ));
    led.record(&core.on_node_frame(
        "mac",
        NodeFrame::AgentStatus {
            agent_id: "p/otter".into(),
            status: AgentStatus::Idle,
            detail: None,
        },
        T0 + 11,
    ));

    let inputs: usize = led.inputs.values().map(Vec::len).sum();
    let overhead = led.framed_tokens.saturating_sub(led.body_tokens);
    println!("\n  token budget report (sample chat, 3 agents, estimate = chars/4)");
    println!("  inputs received (turns)      : {inputs}");
    for (conn, v) in &led.inputs {
        println!(
            "    device {conn}: {} input(s), {} message(s)",
            v.len(),
            v.iter().map(Vec::len).sum::<usize>()
        );
    }
    println!(
        "  message text                 : {} tokens",
        led.body_tokens
    );
    println!(
        "  framing + instructions       : {overhead} tokens, {} per input on average",
        overhead / inputs.max(1)
    );
    println!(
        "  largest per-message framing  : {} tokens",
        led.max_frame_overhead
    );
    println!(
        "  largest system line (brief)  : {} tokens",
        led.max_preface
    );

    assert!(
        led.max_preface <= 150,
        "standing brief must stay at or under 150 tokens"
    );
    assert!(
        led.max_frame_overhead <= 12,
        "framing per message must stay at or under 12 tokens"
    );
    // Short messages make a percentage of text misleading, so the budget is absolute tokens per input.
    assert!(
        overhead / inputs.max(1) <= 30,
        "average framing and instructions per input must stay at or under 30 tokens"
    );
    // Informing messages ride along instead of waking an agent, so this chat needs few turns. A regression that
    // makes every message its own turn would push this up.
    assert!(
        inputs <= 7,
        "this chat must need at most 7 agent turns, got {inputs}"
    );
}
