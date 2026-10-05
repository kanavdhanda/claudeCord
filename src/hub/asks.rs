//! Questions from agents to people, and how they get answered. An ask is a tracked object: it has a number people can
//! refer to (Q3), a state, and exactly one winner. The first valid answer wins, a late one is told who got there first,
//! and an ask only ends by an answer, the agent leaving, or expiry. A status change never deletes one.

use super::core::{HubCore, Queued};
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::protocol::{AgentStatus, HubFrame};

/// The result of a successful answer: which agent asked, and what to carry out.
pub struct AskOutcome {
    pub agent_id: String,
    pub effects: Vec<Effect>,
}

impl HubCore {
    /// An agent asked a question. Records it, shows it to the room and marks the agent as waiting. Sending the same
    /// ask twice (a device retry) is ignored.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_ask(
        &mut self,
        agent_id: &str,
        ask_id: String,
        question: &str,
        options: Option<Vec<String>>,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if self
            .asks_of(&a.project)
            .iter()
            .any(|x| x.id == ask_id && x.agent_id == agent_id)
        {
            return;
        }
        let question = self.scrub(&a, question, now, fx);
        let qn = self.next_number(format!("q:{}", a.project));
        let ask = Ask {
            id: ask_id,
            qn,
            project: a.project.clone(),
            agent_id: agent_id.to_string(),
            question: question.clone(),
            options,
            thread: thread.clone(),
            state: AskState::Open,
            opened: now,
            reminded: false,
        };
        self.asks
            .entry(a.project.clone())
            .or_default()
            .push(ask.clone());
        self.status
            .insert(agent_id.to_string(), (AgentStatus::WaitingInput, None));
        Self::event(&a.project, "status", &a.name, "waiting_input", 0.0, now, fx);
        Self::event(
            &a.project,
            "ask_open",
            &a.name,
            &format!("Q{qn}"),
            0.0,
            now,
            fx,
        );
        fx.push(Effect::Persist(Persist::History {
            project: a.project.clone(),
            thread,
            from: a.name.clone(),
            kind: "ask",
            text: format!("Q{qn}: {question}"),
            at: now,
        }));
        fx.push(Effect::Chat(Chat::Ask {
            project: a.project.clone(),
            agent: a.clone(),
            ask,
        }));
        Self::refresh(&a.project, fx);
    }

    /// Finds an ask by the agent's id or by its display number (`Q3`, any case).
    fn ask_index(&self, project: &str, reference: &str) -> Option<usize> {
        self.asks
            .get(project)?
            .iter()
            .position(|x| x.id == reference || format!("Q{}", x.qn).eq_ignore_ascii_case(reference))
    }

    /// Answers an ask. A person needs at least the operator role. Another agent may answer too, but not its own ask.
    /// The first valid answer wins and everyone after it is told who won.
    pub fn answer_ask(
        &mut self,
        by: &Answerer,
        project: &str,
        reference: &str,
        text: &str,
        now: i64,
    ) -> Result<AskOutcome, Denied> {
        let i = self.ask_index(project, reference).ok_or(Denied::NotFound)?;
        let ask = self.asks[project][i].clone();
        let who = match by {
            Answerer::Human(h) => {
                self.require(project, &h.id, Role::Operator)?;
                self.label(project, h)
            }
            Answerer::Agent(id) => {
                let a = self
                    .agents
                    .get(id)
                    .filter(|a| a.project == project && *id != ask.agent_id)
                    .ok_or(Denied::NotAllowedFor)?;
                a.name.clone()
            }
        };
        match &ask.state {
            AskState::Open => {}
            AskState::Answered { by } => return Err(Denied::AlreadyDone { by: by.clone() }),
            _ => {
                return Err(Denied::AlreadyDone {
                    by: "expiry".into(),
                });
            }
        }
        self.asks.get_mut(project).expect("found above")[i].state =
            AskState::Answered { by: who.clone() };
        let mut fx = Vec::new();
        let asker = self
            .agents
            .get(&ask.agent_id)
            .cloned()
            .ok_or(Denied::NotFound)?;
        self.status
            .insert(ask.agent_id.clone(), (AgentStatus::Thinking, None));
        Self::event(
            project,
            "status",
            &asker.name,
            "thinking",
            0.0,
            now,
            &mut fx,
        );
        Self::event(
            project,
            "ask_done",
            &asker.name,
            &format!("Q{}", ask.qn),
            (now - ask.opened) as f64,
            now,
            &mut fx,
        );
        // The answer goes straight to the asker. If its device is away it waits in the queue instead.
        if !self.send_to(
            &asker,
            HubFrame::Answer {
                agent_id: ask.agent_id.clone(),
                ask_id: ask.id.clone(),
                text: text.into(),
            },
            &mut fx,
        ) {
            self.enqueue(
                &ask.agent_id,
                Queued {
                    from: format!("answer Q{} ({who})", ask.qn),
                    text: text.into(),
                    thread: ask.thread.clone(),
                    reference: None,
                    task_id: None,
                    handoff: None,
                    wake: true,
                    at: now,
                },
            );
        }
        fx.push(Effect::Persist(Persist::History {
            project: project.into(),
            thread: ask.thread.clone(),
            from: who.clone(),
            kind: "answer",
            text: format!("Q{}: {text}", ask.qn),
            at: now,
        }));
        fx.push(Effect::Chat(Chat::Resolved {
            project: project.into(),
            label: format!("Q{}", ask.qn),
            how: format!("answered by {who}"),
        }));
        Self::refresh(project, &mut fx);
        Ok(AskOutcome {
            agent_id: ask.agent_id,
            effects: fx,
        })
    }

    /// Closes every open ask of an agent that has left.
    pub(super) fn cancel_asks_of(&mut self, project: &str, agent_id: &str) {
        for a in self
            .asks
            .get_mut(project)
            .into_iter()
            .flatten()
            .filter(|a| a.agent_id == agent_id && a.state == AskState::Open)
        {
            a.state = AskState::Cancelled;
        }
    }
}
