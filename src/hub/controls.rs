//! Commands that act on agents themselves rather than talk to them: stop one, stop everything, pause and resume,
//! start one on a device, and choose the lead. Each checks the caller's role first. Stopping and pausing need an
//! operator. Stopping everything, spawning and changing the lead need an owner.

use super::briefs;
use super::core::HubCore;
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::protocol::{AgentSpec, AgentStatus, HubFrame};

impl HubCore {
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
        self.require(project, &by.id, Role::Owner)?;
        Ok(match self.conns.get(node) {
            Some(&conn) => (
                true,
                vec![Effect::Send {
                    conn,
                    frame: HubFrame::Spawn { agent: spec },
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
        self.require(project, &by.id, Role::Owner)?;
        let Some(node) = self.pick_node(label) else {
            return Ok((None, vec![]));
        };
        let (_, fx) = self.spawn(by, project, &node, spec)?;
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
