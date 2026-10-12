//! Permission requests from agents, and the people who decide them. Asks for permission are handled outside the model,
//! so deciding one costs the agent no tokens. A request is settled by a standing grant, by a human, or at the terminal,
//! whichever comes first. Every standing grant expires. Only owners can approve high-risk actions or create broad grants.

use super::core::HubCore;
use super::effects::{Chat, Effect, Persist};
use super::model::*;
use crate::protocol::{AgentStatus, HubFrame};

/// Classifies an action as normal or high risk from what kind it is and what it says.
/// ponytail: a small keyword list, not a parser. Upgrade to real command parsing if people find ways around it.
pub(super) fn risk_of(kind: &str, action: &str) -> Risk {
    let high_kind = matches!(kind, "net" | "install" | "push" | "delete");
    let a = action.to_lowercase();
    let high_text = [
        "sudo ",
        "git push",
        "rm -rf",
        "curl | sh",
        "| bash",
        "pip install",
        "npm install",
        "npm i ",
    ]
    .iter()
    .any(|k| a.contains(k));
    if high_kind || high_text {
        Risk::High
    } else {
        Risk::Normal
    }
}

impl HubCore {
    /// An agent asked for permission. Order of checks: never-allowed paths are denied outright, a live grant allows it
    /// quietly, anything else goes to the room with buttons and the agent waits.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_permission(
        &mut self,
        agent_id: &str,
        perm_id: String,
        kind: &str,
        action: &str,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        if self
            .perms_of(&a.project)
            .iter()
            .any(|p| p.id == perm_id && p.agent_id == agent_id)
        {
            return;
        }
        let action = self.scrub(&a, action, now, fx);
        let decide = |allow: bool| HubFrame::Decision {
            agent_id: agent_id.to_string(),
            perm_id: perm_id.clone(),
            allow,
        };
        if Self::touches_sensitive_path(&action) {
            self.send_to(&a, decide(false), fx);
            Self::notice(
                &a.project,
                format!(
                    "Denied {} on {}: it touches a protected file.",
                    a.name, kind
                ),
                true,
                fx,
            );
            Self::audit(
                &a.project,
                "policy",
                format!("denied {} {kind}: protected path", a.name),
                now,
                fx,
            );
            return;
        }
        let risk = risk_of(kind, &action);
        if self
            .matching_grant(&a.project, agent_id, kind, risk, now)
            .is_some()
        {
            self.send_to(&a, decide(true), fx);
            Self::audit(
                &a.project,
                "grant",
                format!("allowed {} {kind}", a.name),
                now,
                fx,
            );
            return;
        }
        let pn = self.next_number(format!("p:{}", a.project));
        let perm = PermRequest {
            id: perm_id,
            pn,
            project: a.project.clone(),
            agent_id: agent_id.into(),
            kind: kind.into(),
            action,
            risk,
            thread,
            state: PermState::Open,
            opened: now,
        };
        self.perms
            .entry(a.project.clone())
            .or_default()
            .push(perm.clone());
        self.status
            .insert(agent_id.into(), (AgentStatus::WaitingInput, None));
        fx.push(Effect::Persist(Persist::History {
            project: a.project.clone(),
            thread: perm.thread.clone(),
            from: a.name.clone(),
            kind: "permission",
            text: format!("P{} {kind}: {}", perm.pn, perm.action),
            at: now,
        }));
        fx.push(Effect::Chat(Chat::Permission {
            project: a.project.clone(),
            agent: a.clone(),
            perm,
        }));
        Self::refresh(&a.project, fx);
    }

    /// A live grant that covers this request, if any. High-risk requests are only covered by grants an owner made.
    fn matching_grant(
        &self,
        project: &str,
        agent_id: &str,
        kind: &str,
        risk: Risk,
        now: i64,
    ) -> Option<&Grant> {
        self.grants.iter().find(|g| {
            g.project == project
                && g.expires_at > now
                && g.agent_id.as_deref().is_none_or(|id| id == agent_id)
                && g.kind.as_deref().is_none_or(|k| k == kind)
                && (risk == Risk::Normal || g.by_owner)
        })
    }

    /// Finds a permission request by the agent's id or its number (`P2`, any case).
    fn perm_index(&self, project: &str, reference: &str) -> Option<usize> {
        self.perms
            .get(project)?
            .iter()
            .position(|x| x.id == reference || format!("P{}", x.pn).eq_ignore_ascii_case(reference))
    }

    /// A person decides a request. Deny and Once need the operator role (high risk needs an owner to allow). Kind and
    /// All create a standing grant, which only an owner may do, and which expires after `ttl_ms` (default one hour).
    pub fn decide_permission(
        &mut self,
        by: &Human,
        project: &str,
        reference: &str,
        decision: Decision,
        ttl_ms: Option<i64>,
        now: i64,
    ) -> Result<Vec<Effect>, Denied> {
        let i = self
            .perm_index(project, reference)
            .ok_or(Denied::NotFound)?;
        let perm = self.perms[project][i].clone();
        let role = self.require(project, &by.id, Role::Operator)?;
        let needs_owner = match decision {
            Decision::Deny => false,
            Decision::Once => perm.risk == Risk::High,
            Decision::Kind | Decision::All => true,
        };
        if needs_owner && role < Role::Owner {
            return Err(Denied::NeedsRole(Role::Owner));
        }
        match &perm.state {
            PermState::Open => {}
            PermState::Allowed { by, .. } | PermState::Denied { by } => {
                return Err(Denied::AlreadyDone { by: by.clone() });
            }
            PermState::Terminal => {
                return Err(Denied::AlreadyDone {
                    by: "the terminal".into(),
                });
            }
            PermState::Expired => {
                return Err(Denied::AlreadyDone {
                    by: "expiry".into(),
                });
            }
        }
        let who = self.label(project, by);
        let ttl = ttl_ms.unwrap_or(self.grant_ttl_ms);
        let (state, how) = match decision {
            Decision::Deny => (
                PermState::Denied { by: who.clone() },
                format!("denied by {who}"),
            ),
            Decision::Once => (
                PermState::Allowed {
                    by: who.clone(),
                    how: "once".into(),
                },
                format!("allowed once by {who}"),
            ),
            Decision::Kind => (
                PermState::Allowed {
                    by: who.clone(),
                    how: "kind".into(),
                },
                format!(
                    "allowed every {} for {} min by {who}",
                    perm.kind,
                    ttl / 60_000
                ),
            ),
            Decision::All => (
                PermState::Allowed {
                    by: who.clone(),
                    how: "all".into(),
                },
                format!("allowed everything for {} min by {who}", ttl / 60_000),
            ),
        };
        self.perms.get_mut(project).expect("found above")[i].state = state;
        let mut fx = Vec::new();
        match decision {
            Decision::Kind => self.grants.push(Grant {
                project: project.into(),
                agent_id: Some(perm.agent_id.clone()),
                kind: Some(perm.kind.clone()),
                expires_at: now + ttl,
                by: who.clone(),
                by_owner: role == Role::Owner,
            }),
            Decision::All => self.grants.push(Grant {
                project: project.into(),
                agent_id: Some(perm.agent_id.clone()),
                kind: None,
                expires_at: now + ttl,
                by: who.clone(),
                by_owner: role == Role::Owner,
            }),
            _ => {}
        }
        if let Some(a) = self.agents.get(&perm.agent_id).cloned() {
            self.send_to(
                &a,
                HubFrame::Decision {
                    agent_id: perm.agent_id.clone(),
                    perm_id: perm.id.clone(),
                    allow: decision != Decision::Deny,
                },
                &mut fx,
            );
            self.status
                .insert(perm.agent_id.clone(), (AgentStatus::Thinking, None));
        }
        Self::audit(
            project,
            &who,
            format!("P{} {} {}: {how}", perm.pn, perm.kind, perm.action),
            now,
            &mut fx,
        );
        fx.push(Effect::Persist(Persist::History {
            project: project.into(),
            thread: perm.thread.clone(),
            from: who.clone(),
            kind: "decision",
            text: format!("P{} {how}", perm.pn),
            at: now,
        }));
        fx.push(Effect::Chat(Chat::Resolved {
            project: project.into(),
            label: format!("P{}", perm.pn),
            how,
        }));
        Self::refresh(project, &mut fx);
        Ok(fx)
    }

    /// The `/grant` command: an owner gives standing permission to one agent (or all agents) for one kind of action
    /// (or all kinds), until it expires. Nothing is allowed forever.
    pub fn grant(
        &mut self,
        by: &Human,
        project: &str,
        agent: Option<&str>,
        kind: Option<&str>,
        ttl_ms: Option<i64>,
        now: i64,
    ) -> Result<Grant, Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let agent_id = match agent {
            Some(name) => Some(
                self.find_by_name(project, name)
                    .ok_or(Denied::NotFound)?
                    .agent_id
                    .clone(),
            ),
            None => None,
        };
        let g = Grant {
            project: project.into(),
            agent_id,
            kind: kind.map(String::from),
            expires_at: now + ttl_ms.unwrap_or(self.grant_ttl_ms),
            by: self.label(project, by),
            by_owner: true,
        };
        self.grants.push(g.clone());
        Ok(g)
    }

    /// Ends every standing permission in a project at once. Owner only.
    pub fn revoke_grants(&mut self, by: &Human, project: &str) -> Result<usize, Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let before = self.grants.len();
        self.grants.retain(|g| g.project != project);
        Ok(before - self.grants.len())
    }

    /// The same prompt was answered at the terminal. Close the chat copy so nobody answers it twice.
    pub(super) fn permission_done_at_terminal(
        &mut self,
        agent_id: &str,
        perm_id: &str,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        let Some(i) = self.perm_index(&a.project, perm_id) else {
            return;
        };
        let p = &mut self.perms.get_mut(&a.project).expect("found above")[i];
        if p.state != PermState::Open {
            return;
        }
        p.state = PermState::Terminal;
        fx.push(Effect::Chat(Chat::Resolved {
            project: a.project.clone(),
            label: format!("P{}", p.pn),
            how: "resolved in terminal".into(),
        }));
    }
}
