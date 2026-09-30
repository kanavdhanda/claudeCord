//! Handoff: how work survives a session running out. Sessions end (the account's session allowance, a full context,
//! a crash), and a fresh session must carry on without replaying the old conversation, which would cost as much as
//! the one that just ran out. So agents save a short structured summary (`context_dump`), and a fresh session asks for
//! it (`pickup`). The hub keeps the latest one per agent and gives it out until a session actually accepts it.
//!
//! Edge cases this file settles:
//! - Finished work is never handed over again: only open tasks and open asks appear in the state line.
//! - A handoff is consumed by acceptance, not by being sent, so a session that dies before reading it does not lose it.
//! - Messages a dead session never accepted are given again at pickup, and only those.
//! - A newer dump replaces an older one. A repeated request for a dump is not sent twice in a short while.
//! - Handoff text is scrubbed for secrets and capped in size so it cannot become the next context problem.

use super::core::{HubCore, Queued};
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::protocol::{HubFrame, UsageKind};

/// Usage percentage at which agents are asked to save a handoff.
pub const DUMP_AT_PCT: u64 = 97;
/// An agent is not asked to dump again within this long.
const DUMP_REPEAT_MS: i64 = 30 * 60_000;
/// Largest handoff accepted, in characters (about 1,500 tokens).
pub const HANDOFF_MAX_CHARS: usize = 6000;

impl HubCore {
    /// The latest handoff saved for an agent, if any.
    pub fn handoff_of(&self, agent_id: &str) -> Option<&Handoff> {
        self.handoffs.get(agent_id)
    }

    /// A usage reading arrived. At the threshold, a session reading goes to every agent (the allowance is shared by all
    /// windows on the account), and a context reading goes only to the agent whose window is filling up.
    pub(super) fn on_usage(
        &mut self,
        agent_id: &str,
        kind: UsageKind,
        pct: u64,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if pct < DUMP_AT_PCT {
            return;
        }
        let targets: Vec<AgentRow> = match kind {
            UsageKind::Context => vec![a.clone()],
            UsageKind::Session => self.agents.values().cloned().collect(),
        };
        let reason = match kind {
            UsageKind::Context => format!("Your context is {pct}% full."),
            UsageKind::Session => format!("The session allowance is {pct}% used."),
        };
        let asked = self.request_dumps(&targets, &reason, now, fx);
        if asked > 0 {
            Self::notice(
                &a.project,
                format!("{reason} Asked {asked} agent(s) to save a handoff."),
                true,
                fx,
            );
        }
    }

    /// Sends the urgent request to each agent not asked recently. It goes straight to the device, ahead of the queue
    /// and even while the agent is held, because it may be the last chance. Returns how many agents were asked.
    fn request_dumps(
        &mut self,
        targets: &[AgentRow],
        reason: &str,
        now: i64,
        fx: &mut Vec<Effect>,
    ) -> usize {
        let mut asked = 0;
        for t in targets {
            if self
                .dump_asked
                .get(&t.agent_id)
                .is_some_and(|at| now - at < DUMP_REPEAT_MS)
            {
                continue;
            }
            let text = format!(
                "URGENT: {reason} Run context_dump now: goal, done, pending, decisions, files, next step. Then stop."
            );
            if self.send_to(
                t,
                HubFrame::Deliver {
                    agent_id: t.agent_id.clone(),
                    from: "system".into(),
                    text,
                    thread: None,
                    msg_id: None,
                },
                fx,
            ) {
                self.dump_asked.insert(t.agent_id.clone(), now);
                self.metrics.inc("dump_requested", 1.0, now);
                asked += 1;
            }
        }
        asked
    }

    /// `/dump`: asks one agent, or all agents in the project, to save a handoff now. Needs the operator role.
    pub fn dump(
        &mut self,
        by: &Human,
        project: &str,
        name: Option<&str>,
        now: i64,
    ) -> Result<(usize, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let targets: Vec<AgentRow> = match name {
            Some(n) => vec![
                self.find_by_name(project, n)
                    .cloned()
                    .ok_or(Denied::NotFound)?,
            ],
            None => self
                .agents_of_project(project)
                .into_iter()
                .cloned()
                .collect(),
        };
        let mut fx = Vec::new();
        // A person asking overrides the repeat guard.
        for t in &targets {
            self.dump_asked.remove(&t.agent_id);
        }
        let n = self.request_dumps(&targets, "A handoff was requested.", now, &mut fx);
        Ok((n, fx))
    }

    /// An agent saved its state. Scrubbed, size-checked, numbered, and kept as the latest, replacing any older one.
    pub(super) fn on_handoff(
        &mut self,
        agent_id: &str,
        text: &str,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if crate::jslen(text) > HANDOFF_MAX_CHARS {
            self.tell(&a, format!("Handoff too long. Keep it under {HANDOFF_MAX_CHARS} characters and run context_dump again."), now, fx);
            return;
        }
        let clean = self.scrub(&a, text, now, fx);
        let seq = self.next_number(format!("h:{agent_id}"));
        self.handoffs.insert(
            agent_id.to_string(),
            Handoff {
                agent_id: agent_id.into(),
                project: a.project.clone(),
                seq,
                text: clean.clone(),
                at: now,
                state: HandoffState::Ready,
            },
        );
        self.metrics.inc("handoffs", 1.0, now);
        fx.push(Effect::Persist(Persist::Handoff {
            project: a.project.clone(),
            agent_id: agent_id.into(),
            seq,
            text: clean.clone(),
            at: now,
        }));
        fx.push(Effect::Persist(Persist::History {
            project: a.project.clone(),
            thread: None,
            from: a.name.clone(),
            kind: "handoff",
            text: format!("handoff #{seq}"),
            at: now,
        }));
        Self::notice(
            &a.project,
            format!(
                "{} saved handoff #{seq} (about {} tokens).",
                a.name,
                clean.chars().count().div_ceil(4)
            ),
            false,
            fx,
        );
    }

    /// A short line of what is still open for an agent: tasks not done and asks not answered. Finished work is left out
    /// on purpose, so a fresh session never redoes or re-answers it.
    fn open_state(&self, a: &AgentRow) -> String {
        let mut parts: Vec<String> = Vec::new();
        for t in self
            .tasks_of(&a.project)
            .iter()
            .filter(|t| t.to_agent == a.agent_id && t.state != TaskState::Done)
        {
            parts.push(format!(
                "{} {}",
                t.id,
                if t.state == TaskState::Accepted {
                    "accepted"
                } else {
                    "assigned"
                }
            ));
        }
        for q in self
            .asks_of(&a.project)
            .iter()
            .filter(|q| q.agent_id == a.agent_id && q.state == AskState::Open)
        {
            parts.push(format!("Q{} open", q.qn));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("state: {}", parts.join("; "))
        }
    }

    /// Builds what a fresh session receives: the ready handoff (if any) and the open state. None when there is nothing
    /// to carry on from, so a clean start costs no extra tokens.
    fn pickup_text(&self, a: &AgentRow, source: Option<&Handoff>) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        if let Some(h) = source {
            lines.push(format!(
                "handoff #{} from the last session:\n{}",
                h.seq, h.text
            ));
        }
        let state = self.open_state(a);
        if !state.is_empty() {
            lines.push(state);
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// A fresh session asked what to carry on from. It gets the ready handoff and the open state, plus anything the
    /// previous session was sent and never accepted. Finished tasks and answered asks are not repeated.
    pub(super) fn on_pickup(&mut self, agent_id: &str, now: i64, fx: &mut Vec<Effect>) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        // Whatever the dead session was handed and never accepted comes back, unless it is a task already done.
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut redo: Vec<Queued> = Vec::new();
        for id in stale {
            if let Some(p) = self.pending.remove(&id) {
                redo.extend(
                    p.items
                        .into_iter()
                        .filter(|q| q.handoff.is_none())
                        .filter(|q| {
                            q.task_id
                                .as_ref()
                                .is_none_or(|t| !self.task_is_done(&a.project, t))
                        }),
                );
            }
        }
        let source = self
            .handoffs
            .get(agent_id)
            .filter(|h| h.state == HandoffState::Ready)
            .cloned();
        // The new session starts clean, so it needs its brief again.
        self.briefed.remove(agent_id);
        if let Some(text) = self.pickup_text(&a, source.as_ref()) {
            self.queue_front(
                agent_id,
                Queued {
                    from: "handoff".into(),
                    text,
                    thread: None,
                    reference: None,
                    task_id: None,
                    handoff: source.map(|h| h.seq),
                    wake: true,
                    at: now,
                },
            );
        }
        for q in redo {
            self.enqueue(agent_id, q);
        }
        self.flush(agent_id, now, fx);
    }

    /// Whether a task is already finished.
    fn task_is_done(&self, project: &str, task_id: &str) -> bool {
        self.tasks_of(project)
            .iter()
            .any(|t| t.id == task_id && t.state == TaskState::Done)
    }

    /// Puts an item at the front of an agent's queue, replacing an older handoff item if one is still waiting there.
    fn queue_front(&mut self, agent_id: &str, q: Queued) {
        let list = self.queues.entry(agent_id.to_string()).or_default();
        list.retain(|x| x.handoff.is_none());
        list.insert(0, q);
    }

    /// `/pickup`: gives a saved handoff to an agent now. With `from`, the handoff of that agent goes to `target`, which
    /// is how work moves to a different window or agent. Needs the operator role.
    pub fn pickup_for(
        &mut self,
        by: &Human,
        project: &str,
        target: &str,
        from: Option<&str>,
        now: i64,
    ) -> Result<Vec<Effect>, Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let to = self
            .find_by_name(project, target)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let src = match from {
            Some(n) => self
                .find_by_name(project, n)
                .cloned()
                .ok_or(Denied::NotFound)?,
            None => to.clone(),
        };
        let h = self
            .handoffs
            .get(&src.agent_id)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let mut fx = Vec::new();
        let text = format!("handoff #{} from {}:\n{}", h.seq, src.name, h.text);
        // Only a handoff given back to its own agent is consumed by accepting it. A transfer leaves the original alone.
        let seq = (src.agent_id == to.agent_id).then_some(h.seq);
        self.queue_front(
            &to.agent_id,
            Queued {
                from: "handoff".into(),
                text,
                thread: None,
                reference: None,
                task_id: None,
                handoff: seq,
                wake: true,
                at: now,
            },
        );
        self.flush(&to.agent_id, now, &mut fx);
        fx.push(Effect::Chat(Chat::Notice {
            project: project.into(),
            text: format!("Handoff #{} from {} given to {}.", h.seq, src.name, to.name),
            mention: false,
        }));
        Ok(fx)
    }
}
