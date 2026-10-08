//! How messages move: from a human to agents, between agents, and onto each agent's queue. Everything bound for an
//! agent waits in its queue and is released as ONE input when the agent can take it, so a burst of messages costs the
//! agent one turn, not one per message. Nothing is ever broadcast: a message goes to who it names, else to the lead.

use super::briefs;
use super::core::{HubCore, Pending, Queued, mentions_name};
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

pub use crate::protocol::{INBOX_DIR, inbox_dir};

/// One short line telling an agent a file arrived: its kind, name, size and where to find it. The content is never
/// put in the agent's context, so an image or a PDF costs tokens only if and when the agent opens it.
pub fn describe_attachment(a: &Attachment, project: &str) -> String {
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
        "[{kind} {name} {kb}KB at {}/{}-{name}]",
        inbox_dir(project),
        crate::agents::text::safe_name(&a.id)
    )
}

/// The text of a message with a reference line added for each attached file.
fn body_with_attachments(text: &str, attachments: &[Attachment], project: &str) -> String {
    let mut body = text.trim().to_string();
    for a in attachments {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&describe_attachment(a, project));
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

    /// What an agent must be told before its next message: its brief, once, or a changed roster. Sent as its own input from the system.
    fn send_preface(&mut self, a: &AgentRow, fx: &mut Vec<Effect>) {
        let mut preface: Vec<String> = Vec::new();
        if self.briefed.insert(a.agent_id.clone()) {
            let b = self.brief_for(a);
            if !b.is_empty() {
                preface.push(b);
            }
            self.roster_dirty.remove(&a.agent_id);
        } else if self.roster_dirty.remove(&a.agent_id) {
            let all = self.agents_of_project(&a.project);
            preface.push(briefs::roster(a, &all));
        }
        if !preface.is_empty() {
            self.send_to(
                a,
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
    }

    /// One message from another agent, sent to the device to be pasted at once and never queued: the device tries for a few seconds and then
    /// says it could not (`NodeFrame::AgentDeliveryFailed`). The sender hears either way (`say.receipt`).
    fn deliver_direct(&mut self, a: &AgentRow, q: Queued, now: i64, fx: &mut Vec<Effect>) {
        self.send_preface(a, fx);
        self.seq += 1;
        let msg_id = format!("m{}", self.seq);
        Self::event(
            &a.project,
            "turn",
            &a.name,
            "",
            q.text.chars().count() as f64,
            now,
            fx,
        );
        Self::event(&a.project, "edge", &q.from, &a.name, 1.0, now, fx);
        self.send_to(
            a,
            HubFrame::Deliver {
                agent_id: a.agent_id.clone(),
                from: q.from.clone(),
                text: q.text.clone(),
                thread: q.thread.clone(),
                msg_id: Some(msg_id.clone()),
            },
            fx,
        );
        self.send_to(
            a,
            HubFrame::Priority {
                agent_id: a.agent_id.clone(),
                msg_id: msg_id.clone(),
                mode: "direct".into(),
            },
            fx,
        );
        self.pending.insert(
            msg_id,
            Pending {
                project: a.project.clone(),
                agent_id: a.agent_id.clone(),
                references: vec![],
                task_ids: vec![],
                handoffs: vec![],
                items: vec![q],
                at: now,
            },
        );
    }

    /// The device could not paste a direct message in time: the sender is told, and the message is gone (a message between agents is not queued).
    pub(super) fn on_delivery_failed(
        &mut self,
        agent_id: &str,
        msg_ids: &[String],
        reason: &str,
        fx: &mut Vec<Effect>,
    ) {
        let name = self.agents.get(agent_id).map(|a| a.name.clone());
        for id in msg_ids {
            if self.pending.get(id).is_none_or(|p| p.agent_id != agent_id) {
                continue;
            }
            let p = self.pending.remove(id).expect("checked above");
            for (sender, say_id) in p.items.iter().filter_map(|q| q.say.clone()) {
                if let Some(s) = self.agents.get(&sender).cloned() {
                    let why = format!(
                        "{} could not take it: {reason}",
                        name.as_deref().unwrap_or("the agent")
                    );
                    self.say_receipt(&s, Some(&say_id), "failed", Some(&why), fx);
                }
            }
        }
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
        self.send_preface(&a, fx);
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
            // The agent that said it is told once everyone it named has it in their terminal.
            for (sender, say_id) in p.items.iter().filter_map(|q| q.say.clone()) {
                let others = self
                    .pending
                    .values()
                    .flat_map(|x| x.items.iter())
                    .chain(self.queues.values().flatten())
                    .any(|q| q.say.as_ref().is_some_and(|(_, i)| *i == say_id));
                if let (false, Some(s)) = (others, self.agents.get(&sender).cloned()) {
                    self.say_receipt(&s, Some(&say_id), "delivered", None, fx);
                }
            }
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

    /// A person reacted to their own message asking for it to go through sooner (`"now"`, or `"steer"` to stop the agent first). It only
    /// does something while that message is delivered but not yet picked up; returns whether a request was sent to the agent's device.
    pub fn prioritise(
        &mut self,
        by: &Human,
        project: &str,
        reference: &str,
        mode: &str,
        fx: &mut Vec<Effect>,
    ) -> Result<bool, Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let Some((msg_id, agent_id)) = self
            .pending
            .iter()
            .find(|(_, p)| p.project == project && p.references.iter().any(|r| r == reference))
            .map(|(id, p)| (id.clone(), p.agent_id.clone()))
        else {
            return Ok(false);
        };
        let Some(a) = self.agents.get(&agent_id).cloned() else {
            return Ok(false);
        };
        Ok(self.send_to(
            &a,
            HubFrame::Priority {
                agent_id,
                msg_id,
                mode: mode.to_string(),
            },
            fx,
        ))
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
        // A reply to the question's own message answers it; so does naming the agent that asked, or, with exactly one question open, any message that
        // names no agent. Either way the answer goes to the agent that asked and to nobody else.
        let implied = if opts.answers_ask.is_none() {
            self.implied_ask(project, text)
        } else {
            None
        };
        if let Some(ask) = opts.answers_ask.or(implied.as_deref()) {
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
        let body = body_with_attachments(text, opts.attachments, project);
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
                    say: None,
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
    pub(super) fn addresses_human(&mut self, project: &str, text: &str) -> bool {
        if HUMAN_MENTION.is_match(text) {
            return true;
        }
        let names: Vec<String> = self
            .members
            .get(project)
            .map(|m| m.values().map(|x| x.name.clone()).collect())
            .unwrap_or_default();
        // A whole name, like a mention of an agent: a person called `k` is not addressed by `@kitchen`, nor an agent's `@name` taken for a person's.
        names
            .iter()
            .any(|n| !n.trim().is_empty() && mentions_name(text, n))
    }

    /// The open question a person's message answers without being a reply to it: the one asked by the single agent it names, or, when it names no
    /// agent at all and exactly one question is open, that one.
    fn implied_ask(&mut self, project: &str, text: &str) -> Option<String> {
        let open: Vec<(String, String)> = self
            .asks_of(project)
            .iter()
            .filter(|a| a.state == AskState::Open)
            .map(|a| (a.id.clone(), a.agent_id.clone()))
            .collect();
        if open.is_empty() {
            return None;
        }
        let named: Vec<String> = self
            .mentioned(project, text, None)
            .into_iter()
            .map(|a| a.agent_id)
            .collect();
        match named.as_slice() {
            [] if open.len() == 1 => Some(open[0].0.clone()),
            [one] => open
                .iter()
                .rev()
                .find(|(_, agent)| agent == one)
                .map(|(id, _)| id.clone()),
            _ => None,
        }
    }

    /// Passes one agent message on to its peers. Mentioned peers get it. If none are mentioned it goes to the lead,
    /// unless the sender is the lead, in which case it only appears in chat. Each agent has its own loop guard.
    pub(super) fn route_agent_message(
        &mut self,
        from: &AgentRow,
        text: &str,
        thread: Option<String>,
        say_id: Option<String>,
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
            self.say_receipt(from, say_id.as_deref(), "posted", None, fx);
            return;
        }
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
        if n >= self.streak_limit {
            let (state, why) = if mentioned.is_empty() {
                ("posted", None)
            } else {
                (
                    "failed",
                    Some(
                        "forwarding is paused after many messages without a reply; a person has to reply first",
                    ),
                )
            };
            self.say_receipt(from, say_id.as_deref(), state, why, fx);
            return;
        }
        // Only naming someone sends a message to an agent. A plain say is for the chat, and nobody else's turn is spent on it.
        let wake = !mentioned.is_empty();
        let targets: Vec<AgentRow> = mentioned;
        let _ = peers;
        if targets.is_empty() {
            self.say_receipt(from, say_id.as_deref(), "posted", None, fx);
        }
        // A message between agents is not queued: each named agent either can take it now or the sender is told it could not.
        let mut failed: Vec<String> = Vec::new();
        for t in targets {
            if !self.conns.contains_key(&t.node_name) {
                failed.push(format!("{} is offline", t.name));
            } else if let Some(why) = hold_reason(self.status_of(&t.agent_id)) {
                failed.push(format!("{} is {why}", t.name));
            } else {
                let q = Queued {
                    from: from.name.clone(),
                    text: text.into(),
                    thread: thread.clone(),
                    reference: None,
                    task_id: None,
                    handoff: None,
                    wake,
                    at: now,
                    say: say_id.clone().map(|id| (from.agent_id.clone(), id)),
                };
                self.deliver_direct(&t, q, now, fx);
            }
        }
        if !failed.is_empty() {
            self.say_receipt(
                from,
                say_id.as_deref(),
                "failed",
                Some(&failed.join("; ")),
                fx,
            );
        }
    }

    /// Tells the agent that said something how it went (see `HubFrame::SayReceipt`). Nothing to say when it did not ask.
    pub(super) fn say_receipt(
        &self,
        to: &AgentRow,
        say_id: Option<&str>,
        state: &str,
        detail: Option<&str>,
        fx: &mut Vec<Effect>,
    ) {
        let Some(say_id) = say_id else { return };
        self.send_to(
            to,
            HubFrame::SayReceipt {
                agent_id: to.agent_id.clone(),
                say_id: say_id.to_string(),
                state: state.to_string(),
                detail: detail.map(String::from),
            },
            fx,
        );
    }
}

impl HubCore {
    /// `claudecord team`: tells the agent who is in its project, with jobs, who leads and who is reachable. It arrives as the agent's next input.
    pub(super) fn on_team(&mut self, agent_id: &str, now: i64, fx: &mut Vec<Effect>) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        let rows: Vec<(AgentRow, String, bool)> = self
            .agents_of_project(&a.project)
            .into_iter()
            .map(|p| {
                let status = format!("{:?}", self.status_of(&p.agent_id)).to_lowercase();
                (p.clone(), status, self.conns.contains_key(&p.node_name))
            })
            .collect();
        let view: Vec<(&AgentRow, &str, bool)> =
            rows.iter().map(|(p, s, o)| (p, s.as_str(), *o)).collect();
        self.queues
            .entry(agent_id.to_string())
            .or_default()
            .push(Queued {
                from: "system".into(),
                text: briefs::team(&view),
                thread: None,
                reference: None,
                task_id: None,
                handoff: None,
                wake: true,
                at: now,
                say: None,
            });
        self.flush(agent_id, now, fx);
    }
}
