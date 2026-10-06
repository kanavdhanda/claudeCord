//! Commands that act on agents themselves rather than talk to them: stop one, stop everything, pause and resume,
//! start one on a device, and choose the lead. Each checks the caller's role first. Stopping and pausing need an
//! operator. Stopping everything, spawning and changing the lead need an owner.

use super::briefs;
use super::core::{HubCore, Queued};
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::protocol::{AgentSpec, AgentStatus, HubFrame};

/// What a request to start an agent can carry besides the agent itself.
#[derive(Debug, Default, Clone)]
pub struct SpawnExtra {
    /// The shell line of a saved startup command, to run instead of the plain agent program.
    pub command: Option<String>,
    /// The code of a `claudecord start` waiting on the machine, so the agent goes in the folder it was run in.
    pub pick: Option<String>,
}

/// Why an agent could not be moved to another project.
#[derive(Debug)]
pub enum MoveError {
    /// The caller may not, or the agent does not exist.
    Denied(Denied),
    /// The move itself is not possible, with the reason to show.
    Refused(String),
}

impl From<Denied> for MoveError {
    fn from(d: Denied) -> Self {
        MoveError::Denied(d)
    }
}

impl HubCore {
    /// Moves a running agent, in place, from one project to another: the same agent and conversation, filed under the other project from now on.
    /// An owner of both projects may. Afterwards it belongs to the new project alone: what is said in the old one no longer reaches it and what it
    /// says goes to the new one's chat only. Its open tasks go back to the old project's lead (or are closed if there is none), its questions
    /// there are cancelled, the lead role passes on if it had it, and it is briefed again on its new team.
    pub fn move_agent(
        &mut self,
        by: &Human,
        from: &str,
        name: &str,
        to: &str,
        now: i64,
    ) -> Result<(AgentRow, Vec<Effect>), MoveError> {
        if from == to {
            return Err(MoveError::Refused(format!("{name} is already in {to}.")));
        }
        self.require(from, &by.id, Role::Owner)?;
        self.require(to, &by.id, Role::Owner)?;
        let a = self
            .find_by_name(from, name)
            .cloned()
            .ok_or(Denied::NotFound)?;
        if self.find_by_name(to, &a.name).is_some() {
            return Err(MoveError::Refused(format!(
                "{to} already has an agent called {}: rename or stop one of them first.",
                a.name
            )));
        }
        if self.agents_of_project(to).len() >= super::core::MAX_AGENTS_PER_PROJECT {
            return Err(MoveError::Refused(format!(
                "{to} has no room for another agent."
            )));
        }
        if !self.conns.contains_key(&a.node_name) {
            return Err(MoveError::Refused(format!(
                "{}'s machine ({}) is not connected, so it cannot be told about the move.",
                a.name, a.node_name
            )));
        }
        let mut fx = Vec::new();
        let id = a.agent_id.clone();
        // Out of the old project: its place in the roster, then the lead role if it held it, then what it was working on.
        self.forget_agent(&id);
        if a.is_lead {
            self.hand_over_lead(from, &mut fx);
        }
        let new_owner = self
            .agents_of_project(from)
            .iter()
            .find(|m| m.is_lead)
            .map(|m| m.agent_id.clone());
        let mut handed = 0;
        for t in self.tasks.get_mut(from).into_iter().flatten() {
            if t.to_agent == id && t.state != TaskState::Done {
                handed += 1;
                t.updated = now;
                match &new_owner {
                    Some(lead) => t.to_agent = lead.clone(),
                    None => {
                        t.state = TaskState::Done;
                        t.summary = Some(format!("{} moved to {to}", a.name));
                    }
                }
            }
        }
        self.cancel_asks_of(from, &id);
        // Into the new one, as its lead if it has none.
        let no_lead = !self.agents_of_project(to).iter().any(|m| m.is_lead);
        let row = AgentRow {
            project: to.to_string(),
            is_lead: no_lead,
            ..a.clone()
        };
        self.agents.insert(id.clone(), row.clone());
        let members = self.by_project.entry(to.to_string()).or_default();
        for m in members.iter() {
            self.roster_dirty.insert(m.clone());
        }
        members.push(id.clone());
        self.roster_dirty.insert(id.clone());
        for m in self.by_project.get(from).cloned().unwrap_or_default() {
            self.roster_dirty.insert(m);
        }
        // Nothing queued for it is from this project: what waited from the old one is dropped, and it is briefed again.
        self.queues.remove(&id);
        self.briefed.remove(&id);
        fx.push(Effect::Persist(Persist::UpsertAgent(row.clone())));
        if row.is_lead {
            fx.push(Effect::Persist(Persist::SetLead {
                project: to.into(),
                agent_id: id.clone(),
            }));
        }
        fx.push(Effect::Chat(Chat::EnsureProject(to.to_string())));
        let who = self.label(from, by);
        Self::audit(from, &who, format!("move {} to {to}", a.name), now, &mut fx);
        Self::audit(
            to,
            &who,
            format!("move {} from {from}", a.name),
            now,
            &mut fx,
        );
        let tasks_note = match handed {
            0 => String::new(),
            n => format!(
                " Its {n} open task(s) went back to {}.",
                match &new_owner {
                    Some(_) => "the lead",
                    None => "nobody (closed: no lead is left)",
                }
            ),
        };
        Self::notice(
            from,
            format!("{} moved to {to}.{tasks_note}", a.name),
            false,
            &mut fx,
        );
        Self::notice(
            to,
            format!("{} joined from {from}.", a.name),
            false,
            &mut fx,
        );
        self.send_to(
            &row,
            HubFrame::Moved {
                agent_id: id.clone(),
                project: to.to_string(),
            },
            &mut fx,
        );
        self.enqueue(
            &id,
            Queued {
                from: "system".into(),
                text: format!(
                    "You moved from project {from} to project {to}. Everything before this was about {from}: its tasks and questions are closed for you, and you can no longer read or post there. From now on you work with {to}."
                ),
                thread: None,
                reference: None,
                task_id: None,
                handoff: None,
                wake: true,
                at: now,
            },
        );
        self.flush(&id, now, &mut fx);
        Self::refresh(from, &mut fx);
        Self::refresh(to, &mut fx);
        Ok((row, fx))
    }

    /// `/stop`: asks one agent's device to quit that agent. Returns whether the device was reachable.
    pub fn stop(
        &mut self,
        by: &Human,
        project: &str,
        name: &str,
        now: i64,
    ) -> Result<(bool, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let a = self
            .find_by_name(project, name)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let mut fx = Vec::new();
        let ok = self.send_to(
            &a,
            HubFrame::Stop {
                agent_id: a.agent_id.clone(),
            },
            &mut fx,
        );
        let who = self.label(project, by);
        Self::audit(project, &who, format!("stop {}", a.name), now, &mut fx);
        Ok((ok, fx))
    }

    /// The project's lead has left: the agent that has been in the project longest takes over, everyone is told with their next delivery, and the
    /// chat says so. Without this a project with no lead sends plain messages to nobody.
    pub(super) fn hand_over_lead(&mut self, project: &str, fx: &mut Vec<Effect>) {
        let Some(next) = self
            .by_project
            .get(project)
            .and_then(|v| v.first())
            .cloned()
        else {
            return;
        };
        for id in self.by_project.get(project).cloned().unwrap_or_default() {
            if let Some(m) = self.agents.get_mut(&id) {
                m.is_lead = id == next;
            }
            self.briefed.remove(&id);
        }
        fx.push(Effect::Persist(Persist::SetLead {
            project: project.into(),
            agent_id: next.clone(),
        }));
    }

    /// Starts one agent over from its base: what was waiting for it is dropped, it gets its brief again, and its device starts its program afresh
    /// (a new session, same name and folder).
    fn reset_agent(&mut self, id: &str, fx: &mut Vec<Effect>) {
        self.queues.remove(id);
        self.briefed.remove(id);
        let gone: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.agent_id == id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            self.pending.remove(&k);
        }
        if let Some(a) = self.agents.get(id).cloned() {
            // It will not remember asking, so its open questions are closed rather than left to be answered for nobody.
            self.cancel_asks_of(&a.project, id);
            self.send_to(
                &a,
                HubFrame::Restart {
                    agent_id: a.agent_id.clone(),
                },
                fx,
            );
        }
    }

    /// `/clear` with no agent named: a fresh chat for a project. Every agent starts over (see `reset_agent`) and the chat channel is cleared. Owner only.
    /// Returns how many agents start over.
    pub fn clear_chat(
        &mut self,
        by: &Human,
        project: &str,
        now: i64,
    ) -> Result<(usize, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let mut fx = Vec::new();
        let ids = self.by_project.get(project).cloned().unwrap_or_default();
        for id in &ids {
            self.reset_agent(id, &mut fx);
        }
        fx.push(Effect::Chat(Chat::Clear(project.into())));
        Self::audit(project, &by.name, "clear".into(), now, &mut fx);
        Ok((ids.len(), fx))
    }

    /// `/clear agent:NAME`: that one agent starts over, with a clean memory. The channel and the other agents are left alone. An operator may do it.
    pub fn clear_agent(
        &mut self,
        by: &Human,
        project: &str,
        name: &str,
        now: i64,
    ) -> Result<Vec<Effect>, Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let a = self
            .find_by_name(project, name)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let mut fx = Vec::new();
        self.reset_agent(&a.agent_id, &mut fx);
        let who = self.label(project, by);
        Self::notice(
            project,
            format!("{} started over, at {who}'s request.", a.name),
            false,
            &mut fx,
        );
        Self::audit(project, &who, format!("clear {}", a.name), now, &mut fx);
        Ok(fx)
    }

    /// `/screen`: asks an agent's machine for what its terminal shows, which is then posted in the chat. Operator.
    pub fn screen(&mut self, by: &Human, project: &str, name: &str) -> Result<Vec<Effect>, Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let a = self
            .find_by_name(project, name)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let mut fx = Vec::new();
        self.send_to(
            &a,
            HubFrame::Screen {
                agent_id: a.agent_id.clone(),
            },
            &mut fx,
        );
        Ok(fx)
    }

    /// `/killall`: asks every device with agents (in one project, or in all) to quit them. Owner only.
    pub fn kill_all(
        &mut self,
        by: &Human,
        project: Option<&str>,
        now: i64,
    ) -> Result<(usize, Vec<Effect>), Denied> {
        let scope = project.unwrap_or("");
        self.require(scope, &by.id, Role::Owner)?;
        let mut fx = Vec::new();
        let targets: Vec<&AgentRow> = self
            .agents
            .values()
            .filter(|a| project.is_none_or(|p| a.project == p))
            .collect();
        let mut nodes: Vec<&str> = Vec::new();
        for a in &targets {
            if !nodes.contains(&a.node_name.as_str()) {
                nodes.push(&a.node_name);
            }
        }
        for n in nodes {
            if let Some(&conn) = self.conns.get(n) {
                fx.push(Effect::Send {
                    conn,
                    frame: HubFrame::Killall {
                        project: project.map(String::from),
                    },
                });
            }
        }
        let count = targets.len();
        Self::audit(scope, &by.name, "killall".into(), now, &mut fx);
        Ok((count, fx))
    }

    /// `/pause` and `/resume`: holds or releases agents. Held agents keep their messages queued on the hub, and
    /// resuming releases each queue as one input. Operator needed. With a name it affects one agent, else the project.
    pub fn hold(
        &mut self,
        by: &Human,
        on: bool,
        project: &str,
        name: Option<&str>,
        now: i64,
    ) -> Result<(usize, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Operator)?;
        let mut fx = Vec::new();
        let low = name.map(str::to_lowercase);
        let targets: Vec<AgentRow> = self
            .agents_of_project(project)
            .into_iter()
            .filter(|a| low.as_ref().is_none_or(|n| a.name.to_lowercase() == *n))
            .cloned()
            .collect();
        for a in &targets {
            self.send_to(
                a,
                HubFrame::Hold {
                    on,
                    agent_id: Some(a.agent_id.clone()),
                    project: None,
                },
                &mut fx,
            );
            self.status.insert(
                a.agent_id.clone(),
                (
                    if on {
                        AgentStatus::Paused
                    } else {
                        AgentStatus::Idle
                    },
                    None,
                ),
            );
            if !on {
                self.flush(&a.agent_id, now, &mut fx);
            }
        }
        Self::refresh(project, &mut fx);
        Ok((targets.len(), fx))
    }

    /// `/spawn`: asks a device to start an agent. The device only starts agents in folders it registered itself.
    pub fn spawn(
        &self,
        by: &Human,
        project: &str,
        node: &str,
        spec: AgentSpec,
    ) -> Result<(bool, Vec<Effect>), Denied> {
        self.spawn_with(by, project, node, spec, SpawnExtra::default())
    }

    /// Like `spawn`, with what a request can add: a saved startup command, and the `claudecord start` waiting on that machine.
    pub fn spawn_with(
        &self,
        by: &Human,
        project: &str,
        node: &str,
        spec: AgentSpec,
        extra: SpawnExtra,
    ) -> Result<(bool, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        Ok(match self.conns.get(node) {
            Some(&conn) => (
                true,
                vec![Effect::Send {
                    conn,
                    frame: HubFrame::Spawn {
                        agent: spec,
                        command: extra.command,
                        pick: extra.pick,
                    },
                }],
            ),
            None => (false, vec![]),
        })
    }

    /// `/raw`: sends text to an agent's terminal exactly as typed, with no header, so the harness sees it as if a person had
    /// typed it. This is how harness commands (such as `/compact` in one program or `@` references in another) get through
    /// without claudeCord knowing about any of them. Ordinary messages are always delivered as data behind a header, so
    /// they can never trigger a command by accident. Owner only, because a raw command can change what the agent is
    /// allowed to do. The device still waits for a safe moment, and the use is recorded. Returns whether the device was reachable.
    pub fn raw_input(
        &mut self,
        by: &Human,
        project: &str,
        agent: &str,
        text: &str,
        now: i64,
    ) -> Result<(bool, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let a = self
            .find_by_name(project, agent)
            .cloned()
            .ok_or(Denied::NotFound)?;
        let mut fx = Vec::new();
        let text: String = crate::agents::text::strip_control(text)
            .chars()
            .take(2000)
            .collect();
        let sent = self.send_to(
            &a,
            HubFrame::Raw {
                agent_id: a.agent_id.clone(),
                text: text.clone(),
            },
            &mut fx,
        );
        Self::audit(
            project,
            &by.name,
            format!("raw to {}: {text}", a.name),
            now,
            &mut fx,
        );
        Ok((sent, fx))
    }

    /// Which connected machine should take a new agent: one with a free slot (and the asked-for label, if any), the one
    /// running the fewest agents, ties broken by name so the choice is repeatable. A machine that never said what it can
    /// take is assumed to take eight. None if nothing fits.
    pub fn pick_node(&self, label: Option<&str>) -> Option<String> {
        self.devices()
            .into_iter()
            .filter(|d| d.connected)
            .filter(|d| label.is_none_or(|l| d.labels.iter().any(|x| x == l)))
            .filter(|d| (d.agents.len() as u64) < d.max_agents.unwrap_or(8))
            .min_by_key(|d| (d.agents.len(), d.node.clone()))
            .map(|d| d.node)
    }

    /// `/spawn` without naming a machine: the hub picks one by `pick_node` and asks it to start the agent. Returns the
    /// machine chosen, or None if no machine had room. The machine still decides whether it knows the project's folder.
    pub fn spawn_auto(
        &self,
        by: &Human,
        project: &str,
        spec: AgentSpec,
        label: Option<&str>,
    ) -> Result<(Option<String>, Vec<Effect>), Denied> {
        self.spawn_auto_with(by, project, spec, label, SpawnExtra::default())
    }

    /// Like `spawn_auto`, with a saved startup command to run instead of the plain agent program.
    pub fn spawn_auto_with(
        &self,
        by: &Human,
        project: &str,
        spec: AgentSpec,
        label: Option<&str>,
        extra: SpawnExtra,
    ) -> Result<(Option<String>, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let Some(node) = self.pick_node(label) else {
            return Ok((None, vec![]));
        };
        let (_, fx) = self.spawn_with(by, project, &node, spec, extra)?;
        Ok((Some(node), fx))
    }

    /// Chooses which agent leads a project and tells every agent the new arrangement with its next delivery. Owner only.
    pub fn set_lead(
        &mut self,
        by: &Human,
        project: &str,
        agent_id: &str,
    ) -> Result<(AgentRow, Vec<Effect>), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        self.agents
            .get(agent_id)
            .filter(|a| a.project == project)
            .ok_or(Denied::NotFound)?;
        let mut fx = vec![Effect::Persist(Persist::SetLead {
            project: project.into(),
            agent_id: agent_id.into(),
        })];
        for id in self.by_project.get(project).cloned().unwrap_or_default() {
            if let Some(m) = self.agents.get_mut(&id) {
                m.is_lead = id == agent_id;
            }
            // Everyone gets their new brief with the next delivery.
            self.briefed.remove(&id);
        }
        let lead = self.agents.get(agent_id).cloned().ok_or(Denied::NotFound)?;
        let _ = briefs::roster;
        Self::notice(
            project,
            format!("{} now leads {project}.", lead.name),
            false,
            &mut fx,
        );
        Self::refresh(project, &mut fx);
        Ok((lead, fx))
    }
}
