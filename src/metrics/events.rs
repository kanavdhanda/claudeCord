//! Turning the stored events (see `crate::hub::Persist::Event`) into what the dashboard's graphs show: who talks to whom, each agent's
//! state over time, how work flows from task to done and how long questions wait, and what the turns cost. Pure functions of the event
//! list and the clock, so every number can be checked with made-up events.

use crate::store::EventRow;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Characters per token, roughly, for English and code. The hub never sees a model's real token counts (agents run in their own terminals),
/// so cost is shown as turns, which is what really drives it, plus this estimate of the input.
const CHARS_PER_TOKEN: f64 = 4.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub n: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    pub name: String,
    /// "agent" or "human".
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub state: String,
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lane {
    pub project: String,
    pub agent: String,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Bucket {
    pub t: i64,
    pub turns: u32,
    pub tokens: u64,
    pub messages: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentCost {
    pub agent: String,
    pub turns: u32,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Asks {
    pub opened: u32,
    pub answered: u32,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskFlow {
    pub agent: String,
    pub assigned: u32,
    pub accepted: u32,
    pub done: u32,
    pub avg_cycle_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub since: i64,
    pub now: i64,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub lanes: Vec<Lane>,
    pub buckets: Vec<Bucket>,
    pub cost: Vec<AgentCost>,
    pub asks: Asks,
    pub tasks: Vec<TaskFlow>,
}

/// A person's header label such as `kd (owner)` is just `kd` in the graph.
fn plain(name: &str) -> &str {
    name.split(" (").next().unwrap_or(name).trim()
}

/// The value below which `q` of the sorted numbers fall (nearest rank).
fn percentile(sorted: &[i64], q: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

/// Everything the graphs need from `events` between `since` and `now`, with the time series in buckets of `bucket_ms`.
pub fn summarize(events: &[EventRow], since: i64, now: i64, bucket_ms: i64) -> Summary {
    let bucket_ms = bucket_ms.max(1);
    let n_buckets = ((now - since) / bucket_ms + 1).clamp(1, 2000) as usize;
    let mut buckets: Vec<Bucket> = (0..n_buckets)
        .map(|i| Bucket {
            t: since + i as i64 * bucket_ms,
            ..Default::default()
        })
        .collect();
    let slot = |at: i64| (((at - since) / bucket_ms).max(0) as usize).min(n_buckets - 1);

    let mut edges: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut agents: BTreeSet<String> = BTreeSet::new();
    let mut people: BTreeSet<String> = BTreeSet::new();
    let mut states: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    let mut cost: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    let mut asks = Asks::default();
    let mut waits: Vec<i64> = Vec::new();
    let mut tasks: BTreeMap<String, (u32, u32, u32, Vec<i64>)> = BTreeMap::new();

    for (at, project, kind, a, b, n) in events.iter().filter(|e| e.0 >= since && e.0 <= now) {
        match kind.as_str() {
            "status" => {
                agents.insert(a.clone());
                states
                    .entry((project.clone(), a.clone()))
                    .or_default()
                    .push((*at, b.clone()));
            }
            "edge" => {
                let from = plain(a);
                if from == "system" || from.is_empty() {
                    continue;
                }
                agents.insert(b.clone());
                *edges.entry((from.to_string(), b.clone())).or_default() += 1;
                buckets[slot(*at)].messages += 1;
                people.insert(from.to_string());
            }
            "turn" => {
                agents.insert(a.clone());
                let tokens = (n / CHARS_PER_TOKEN).ceil() as u64;
                buckets[slot(*at)].turns += 1;
                buckets[slot(*at)].tokens += tokens;
                let c = cost.entry(a.clone()).or_default();
                c.0 += 1;
                c.1 += tokens;
            }
            "ask_open" => asks.opened += 1,
            "ask_done" => {
                asks.answered += 1;
                waits.push(*n as i64);
            }
            "task" => {
                let t = tasks.entry(a.clone()).or_default();
                match b.as_str() {
                    "assigned" => t.0 += 1,
                    "accepted" => t.1 += 1,
                    "done" => {
                        t.2 += 1;
                        t.3.push(*n as i64);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Anyone who only ever sent messages and is not an agent is a person.
    let nodes: Vec<Node> = agents
        .iter()
        .map(|a| Node {
            name: a.clone(),
            kind: "agent",
        })
        .chain(
            people
                .iter()
                .filter(|p| !agents.contains(*p))
                .map(|p| Node {
                    name: p.clone(),
                    kind: "human",
                }),
        )
        .collect();

    // A lane is each state an agent was in until the next change; the last lasts until now, and repeated states are one stretch.
    let lanes: Vec<Lane> = states
        .into_iter()
        .map(|((project, agent), list)| {
            let mut segments: Vec<Segment> = Vec::new();
            for (i, (at, state)) in list.iter().enumerate() {
                let end = list.get(i + 1).map_or(now, |x| x.0);
                match segments.last_mut() {
                    Some(last) if last.state == *state => last.to = end,
                    _ => segments.push(Segment {
                        state: state.clone(),
                        from: *at,
                        to: end,
                    }),
                }
            }
            Lane {
                project,
                agent,
                segments,
            }
        })
        .collect();

    waits.sort_unstable();
    asks.p50_ms = percentile(&waits, 0.5);
    asks.p95_ms = percentile(&waits, 0.95);
    Summary {
        since,
        now,
        nodes,
        edges: edges
            .into_iter()
            .map(|((from, to), n)| Edge { from, to, n })
            .collect(),
        lanes,
        buckets,
        cost: cost
            .into_iter()
            .map(|(agent, (turns, tokens))| AgentCost {
                agent,
                turns,
                tokens,
            })
            .collect(),
        asks,
        tasks: tasks
            .into_iter()
            .map(|(agent, (assigned, accepted, done, cycles))| TaskFlow {
                agent,
                assigned,
                accepted,
                done,
                avg_cycle_ms: (!cycles.is_empty())
                    .then(|| cycles.iter().sum::<i64>() / cycles.len() as i64),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: i64, kind: &str, a: &str, b: &str, n: f64) -> EventRow {
        (at, "p".into(), kind.into(), a.into(), b.into(), n)
    }

    #[test]
    fn edges_count_messages_and_name_people_without_their_role() {
        let e = vec![
            ev(10, "edge", "kd (owner)", "otter", 1.0),
            ev(11, "edge", "kd (owner)", "otter", 1.0),
            ev(12, "edge", "otter", "mole", 1.0),
            ev(13, "edge", "system", "mole", 1.0),
        ];
        let s = summarize(&e, 0, 100, 50);
        assert_eq!(
            s.edges
                .iter()
                .find(|x| x.from == "kd" && x.to == "otter")
                .unwrap()
                .n,
            2
        );
        assert!(s.edges.iter().all(|x| x.from != "system"));
        assert!(s.nodes.contains(&Node {
            name: "kd".into(),
            kind: "human"
        }));
        assert!(s.nodes.contains(&Node {
            name: "otter".into(),
            kind: "agent"
        }));
        assert_eq!(s.buckets.iter().map(|b| b.messages).sum::<u32>(), 3);
    }

    #[test]
    fn lanes_run_until_the_next_change_and_merge_repeats() {
        let e = vec![
            ev(0, "status", "otter", "idle", 0.0),
            ev(10, "status", "otter", "thinking", 0.0),
            ev(20, "status", "otter", "thinking", 0.0),
            ev(30, "status", "otter", "offline", 0.0),
        ];
        let s = summarize(&e, 0, 100, 50);
        let segs = &s.lanes[0].segments;
        assert_eq!(
            segs.iter()
                .map(|x| (x.state.as_str(), x.from, x.to))
                .collect::<Vec<_>>(),
            vec![("idle", 0, 10), ("thinking", 10, 30), ("offline", 30, 100)]
        );
    }

    #[test]
    fn asks_report_waits_and_tasks_report_cycles() {
        let e = vec![
            ev(1, "ask_open", "otter", "Q1", 0.0),
            ev(2, "ask_open", "otter", "Q2", 0.0),
            ev(3, "ask_done", "otter", "Q1", 1000.0),
            ev(4, "ask_done", "otter", "Q2", 9000.0),
            ev(5, "task", "mole", "assigned", 0.0),
            ev(6, "task", "mole", "accepted", 0.0),
            ev(7, "task", "mole", "done", 4000.0),
        ];
        let s = summarize(&e, 0, 100, 50);
        assert_eq!((s.asks.opened, s.asks.answered), (2, 2));
        assert_eq!((s.asks.p50_ms, s.asks.p95_ms), (Some(1000), Some(9000)));
        assert_eq!(s.tasks[0].avg_cycle_ms, Some(4000));
        assert_eq!(
            (s.tasks[0].assigned, s.tasks[0].accepted, s.tasks[0].done),
            (1, 1, 1)
        );
    }

    #[test]
    fn turns_and_estimated_tokens_add_up_per_agent_and_over_time() {
        let e = vec![
            ev(10, "turn", "otter", "", 400.0),
            ev(70, "turn", "otter", "", 40.0),
        ];
        let s = summarize(&e, 0, 100, 50);
        assert_eq!((s.cost[0].turns, s.cost[0].tokens), (2, 110));
        assert_eq!(s.buckets[0].turns, 1);
        assert_eq!(s.buckets[1].tokens, 10);
    }

    #[test]
    fn events_outside_the_range_are_ignored() {
        let e = vec![
            ev(5, "turn", "otter", "", 40.0),
            ev(500, "turn", "otter", "", 40.0),
        ];
        let s = summarize(&e, 10, 100, 50);
        assert!(s.cost.is_empty());
    }
}
