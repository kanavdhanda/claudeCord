//! How messages move: from a human to agents, between agents, and onto each agent's queue. Everything bound for an
//! agent waits in its queue and is released as ONE input when the agent can take it, so a burst of messages costs the
//! agent one turn, not one per message. Nothing is ever broadcast: a message goes to who it names, else to the lead.

use super::briefs;
use super::core::{HubCore, Pending, Queued};
use super::effects::{Effect, Persist};
use super::model::*;
use crate::agents::text::strip_control;
use crate::protocol::{AgentStatus, HubFrame};
use regex::Regex;
use std::sync::LazyLock;

/// Why an agent in this status cannot take a message right now, or None if it can.
pub(super) fn hold_reason(s: AgentStatus) -> Option<&'static str> {
    match s {
        AgentStatus::Paused => Some("paused"),
        AgentStatus::Limited => Some("at a usage limit"),
        AgentStatus::Offline => Some("offline"),
        AgentStatus::Starting => Some("still starting"),
        _ => None,
    }
}

/// The status as the lower-case word used in metrics.
pub(super) fn status_name(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Starting => "starting",
        AgentStatus::Idle => "idle",
        AgentStatus::Thinking => "thinking",
        AgentStatus::Executing => "executing",
        AgentStatus::WaitingInput => "waiting_input",
        AgentStatus::Paused => "paused",
        AgentStatus::Limited => "limited",
        AgentStatus::Offline => "offline",
    }
}

/// Where on the device an attached file is placed, relative to the project folder.
pub const INBOX_DIR: &str = ".claudecord/inbox";

/// One short line telling an agent a file arrived: its kind, name, size and where to find it. The content is never
/// put in the agent's context, so an image or a PDF costs tokens only if and when the agent opens it.
pub fn describe_attachment(a: &Attachment) -> String {
    let kind = if a.mime.starts_with("image/") {
        "image"
    } else if a.mime == "application/pdf" {
        "pdf"
    } else {
        "file"
    };
    let name = crate::agents::text::safe_name(&a.name);
    let kb = a.size.div_ceil(1024);
    format!(
        "[{kind} {name} {kb}KB at {INBOX_DIR}/{}-{name}]",
        crate::agents::text::safe_name(&a.id)
    )
}

/// The text of a message with a reference line added for each attached file.
fn body_with_attachments(text: &str, attachments: &[Attachment]) -> String {
    let mut body = text.trim().to_string();
    for a in attachments {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&describe_attachment(a));
    }
    body
}

/// A person's display name made safe to put in a header: no control characters, no line breaks, bounded length.
pub(super) fn clean_label(name: &str) -> String {
    strip_control(name)
        .replace('\n', " ")
        .chars()
        .take(40)
        .collect()
}

static HUMAN_MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)@(?:engineer|human|owner)(?-u:\b)").expect("pattern"));

impl HubCore {
    /// Puts a message on an agent's queue. Exact duplicates already waiting are dropped.
    pub(super) fn enqueue(&mut self, agent_id: &str, q: Queued) {
        let list = self.queues.entry(agent_id.to_string()).or_default();
        if list
            .iter()
            .any(|x| x.from == q.from && x.text == q.text && x.thread == q.thread)
        {
            return;
        }
        list.push(q);
    }

    /// Releases an agent's queue as one input if the agent can take it now. Anything the agent needs to know first
    /// (its brief once, a changed roster) rides along in the same input. Does nothing if there is nothing to send, the
    /// agent is on hold, or its device is not connected, in which case the messages simply keep waiting.
    pub(super) fn flush(&mut self, agent_id: &str, now: i64, fx: &mut Vec<Effect>) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if hold_reason(self.status_of(agent_id)).is_some() || !self.conns.contains_key(&a.node_name)
        {
            return;
        }
        if self.queues.get(agent_id).is_none_or(Vec::is_empty) {
            return;
        }
        // Informing-only items wait for something worth a turn, up to a limit. Waking an agent costs far more than the words.
        let queue = &self.queues[agent_id];
        let oldest = queue.iter().map(|q| q.at).min().unwrap_or(now);
        if !queue.iter().any(|q| q.wake) && now - oldest < super::core::RIDE_MAX_MS {
            return;
        }
        let items = self.queues.remove(agent_id).unwrap_or_default();
        let mut preface: Vec<String> = Vec::new();
        if self.briefed.insert(agent_id.to_string()) {
            let b = self.brief_for(&a);
            if !b.is_empty() {
                preface.push(b);
            }
            self.roster_dirty.remove(agent_id);
        } else if self.roster_dirty.remove(agent_id) {
            let all = self.agents_of_project(&a.project);
            preface.push(briefs::roster(&a, &all));
        }
        if !preface.is_empty() {
            self.send_to(
                &a,
                HubFrame::Deliver {
                    agent_id: a.agent_id.clone(),
                    from: "system".into(),
                    text: preface.join(" "),
                    thread: None,
                    msg_id: None,
                },
                fx,
            );
        }
        self.seq += 1;
        let msg_id = format!("m{}", self.seq);
        let last = items.len() - 1;
        let mut pending = Pending {
            project: a.project.clone(),
            agent_id: a.agent_id.clone(),
            references: vec![],
            task_ids: vec![],
            handoffs: vec![],
            items: vec![],
            at: now,
        };
        // One wake-up is one turn, however many messages ride in it; the characters stand in for the input it will cost.
        let chars: usize = items.iter().map(|q| q.text.chars().count()).sum();
        Self::event(&a.project, "turn", &a.name, "", chars as f64, now, fx);
        for q in &items {
            Self::event(&a.project, "edge", &q.from, &a.name, 1.0, now, fx);
        }
        for (i, q) in items.into_iter().enumerate() {
            pending.references.extend(q.reference.clone());
            pending.task_ids.extend(q.task_id.clone());
            pending.handoffs.extend(q.handoff);
            pending.items.push(q.clone());
            // Only the last frame of the batch carries the id: accepting the batch accepts everything in it.
            let id = (i == last).then(|| msg_id.clone());
            self.send_to(
                &a,
                HubFrame::Deliver {
                    agent_id: a.agent_id.clone(),
                    from: q.from,
                    text: q.text,
                    thread: q.thread,
                    msg_id: id,
                },
                fx,
            );
        }
        // Everything stays pending until the device says the agent started on it, so a session that dies first can
        // be given it again.
        self.pending.insert(msg_id, pending);
    }

    /// A device reported it started on a delivery: confirm the human message and accept the tasks that came with it.
    pub(super) fn on_accepted(
        &mut self,
        agent_id: &str,
        msg_ids: &[String],
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        for id in msg_ids {
            if self.pending.get(id).is_none_or(|p| p.agent_id != agent_id) {
                continue;
            }
            let p = self.pending.remove(id).expect("checked above");
            self.metrics.accepted((now - p.at) as f64, now);
            for reference in p.references {
                fx.push(Effect::Chat(super::effects::Chat::Confirm {
                    project: p.project.clone(),
                    reference,
                    agent_name: a.name.clone(),
                }));
            }
            for seq in p.handoffs {
                if let Some(h) = self.handoffs.get_mut(agent_id).filter(|h| h.seq == seq) {
                    h.state = HandoffState::Consumed;
                }
            }
            for task_id in p.task_ids {
                self.set_task(&p.project, &task_id, TaskState::Accepted, None, now, fx);
                self.metrics.inc("task_accepted", 1.0, now);
                Self::notice(
                    &p.project,
                    format!("{} accepted {task_id}.", a.name),
                    false,
                    fx,
                );
            }
        }
    }

    /// The agents a human message should go to: those it mentions, otherwise the lead (or the first agent).
    pub(super) fn pick_targets(&mut self, project: &str, text: &str) -> Vec<AgentRow> {
        let mentioned = self.mentioned(project, text, None);
        if !mentioned.is_empty() {
            return mentioned;
        }
        let agents = self.agents_of_project(project);
        agents
            .iter()
            .find(|a| a.is_lead)
            .or(agents.first())
            .map(|a| (*a).clone())
            .into_iter()
            .collect()
    }

    /// A message from a human. They need at least the operator role. A reply to an ask counts as its answer. Anything
    /// else is queued for the agents it names, or the lead, under the sender's name and role.
    pub fn human_message(
        &mut self,
        by: &Human,
        project: &str,
        text: &str,
        opts: &MessageOpts,
        now: i64,
    ) -> Result<(RouteResult, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let mut fx = Vec::new();
        // A person speaking lifts every agent's loop guard in the project.
        for id in self.by_project.get(project).cloned().unwrap_or_default() {
            self.streak.remove(&id);
        }
        if let Some(ask) = opts.answers_ask {
            let asker = self.answer_ask(&Answerer::Human(by.clone()), project, ask, text, now)?;
            let name = self
                .agents
                .get(&asker.agent_id)
                .map(|a| a.name.clone())
                .unwrap_or_default();
            return Ok((
                RouteResult {
                    targets: vec![name],
                    ..Default::default()
                },
                asker.effects,
            ));
        }
        let from = self.label(project, by);
        let body = body_with_attachments(text, opts.attachments);
        fx.push(Effect::Persist(Persist::History {
            project: project.into(),
            thread: opts.thread.map(String::from),
            from: from.clone(),
            kind: "human",
            text: body.clone(),
            at: now,
        }));
        let targets = self.pick_targets(project, text);
        Ok((
            self.queue_for(
                &targets,
                &from,
                &body,
                opts.thread,
                opts.reference,
                now,
                &mut fx,
            ),
            fx,
        ))
    }

    /// A short aside from a human. It reaches the named agent (or the lead) like a message, but it is not tracked, not
    /// confirmed and not a new instruction: the agent answers briefly and carries on.
    pub fn btw(
        &mut self,
        by: &Human,
        project: &str,
        text: &str,
        target: Option<&str>,
        now: i64,
    ) -> Result<(RouteResult, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let mut fx = Vec::new();
        let to = match target {
            Some(name) => vec![
                self.find_by_name(project, name)
                    .cloned()
                    .ok_or(Denied::NotFound)?,
            ],
            None => self.pick_targets(project, text),
        };
        let from = format!("{} (btw)", clean_label(&by.name));
        Ok((
            self.queue_for(&to, &from, text.trim(), None, None, now, &mut fx),
            fx,
        ))
    }

    /// Queues one message for each target and releases the queues. Reports who is offline or on hold.
    #[allow(clippy::too_many_arguments)]
    fn queue_for(
        &mut self,
        targets: &[AgentRow],
        from: &str,
        text: &str,
        thread: Option<&str>,
        reference: Option<&str>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) -> RouteResult {
        let mut res = RouteResult {
            targets: targets.iter().map(|t| t.name.clone()).collect(),
            ..Default::default()
        };
        if !targets.is_empty() {
            self.metrics.inc("msg_human", 1.0, now);
        }
        for t in targets {
            self.enqueue(
                &t.agent_id,
                Queued {
                    from: from.into(),
                    text: text.into(),
                    thread: thread.map(String::from),
                    reference: reference.map(String::from),
                    task_id: None,
                    handoff: None,
                    wake: true,
                    at: now,
                },
            );
            if !self.conns.contains_key(&t.node_name) {
                res.offline.push(t.name.clone());
            } else if let Some(why) = hold_reason(self.status_of(&t.agent_id)) {
                res.held.push((t.name.clone(), why));
            }
            if let Some(r) = reference {
                fx.push(Effect::AcceptCheck {
                    agent_id: t.agent_id.clone(),
                    reference: r.into(),
                    after_ms: self.accept_timeout_ms,
                });
            }
            self.flush(&t.agent_id, now, fx);
        }
        res
    }

    /// If an agent has not picked a message up in time, say so instead of leaving the human guessing.
    pub fn accept_check(&self, agent_id: &str, reference: &str) -> Vec<Effect> {
        let mut fx = Vec::new();
        let in_flight = self
            .pending
            .values()
            .any(|p| p.agent_id == agent_id && p.references.iter().any(|r| r == reference));
        let queued = self
            .queues
            .get(agent_id)
            .is_some_and(|q| q.iter().any(|x| x.reference.as_deref() == Some(reference)));
        let Some(a) = self.agents.get(agent_id).filter(|_| in_flight || queued) else {
            return fx;
        };
        let st = self.status_of(agent_id);
        let why = hold_reason(st).unwrap_or_else(|| status_name(st));
        Self::notice(
            &a.project,
            format!(
                "{} has not picked up your message yet ({why}). It stays queued.",
                a.name
            ),
            false,
            &mut fx,
        );
        fx
    }

    /// Whether an agent's text speaks to a person, which ends agent-to-agent forwarding for that message.
    fn addresses_human(&mut self, project: &str, text: &str) -> bool {
        if HUMAN_MENTION.is_match(text) {
            return true;
        }
        let names: Vec<String> = self
            .members
            .get(project)
            .map(|m| m.values().map(|x| x.name.clone()).collect())
            .unwrap_or_default();
        names.iter().any(|n| {
            text.to_lowercase()
                .contains(&format!("@{}", n.to_lowercase()))
        })
    }

    /// Passes one agent message on to its peers. Mentioned peers get it. If none are mentioned it goes to the lead,
    /// unless the sender is the lead, in which case it only appears in chat. Each agent has its own loop guard.
    pub(super) fn route_agent_message(
        &mut self,
        from: &AgentRow,
        text: &str,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let peers: Vec<AgentRow> = self
            .agents_of_project(&from.project)
            .into_iter()
            .filter(|a| a.agent_id != from.agent_id)
            .cloned()
            .collect();
        if peers.is_empty() {
            return;
        }
        let to_human = self.addresses_human(&from.project, text);
        let mentioned = self.mentioned(&from.project, text, Some(&from.agent_id));
        let n = self.streak.get(&from.agent_id).copied().unwrap_or(0) + 1;
        self.streak.insert(from.agent_id.clone(), n);
        if n == self.streak_limit {
            Self::notice(
                &from.project,
                format!(
                    "{} has sent many messages without input. Pausing its forwarding until someone replies.",
                    from.name
                ),
                true,
                fx,
            );
        }
        if n >= self.streak_limit || to_human {
            return;
        }
        // Naming someone says a reply is wanted. A plain say is information and rides along with the next real turn.
        let wake = !mentioned.is_empty();
        let targets: Vec<AgentRow> = if wake {
            mentioned
        } else if from.is_lead {
            vec![]
        } else {
            peers.into_iter().filter(|p| p.is_lead).collect()
        };
        for t in targets {
            self.enqueue(
                &t.agent_id,
                Queued {
                    from: from.name.clone(),
                    text: text.into(),
                    thread: thread.clone(),
                    reference: None,
                    task_id: None,
                    handoff: None,
                    wake,
                    at: now,
                },
            );
            self.flush(&t.agent_id, now, fx);
        }
    }
}
