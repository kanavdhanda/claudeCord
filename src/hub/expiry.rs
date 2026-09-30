//! Time passing. Called now and then by the shell with the current time. Reminds the room about asks nobody has
//! answered, expires old asks and permission requests, and drops grants that ran out. The agent is always told, so
//! it never waits on something that can no longer be answered.

use super::core::HubCore;
use super::effects::{Chat, Effect};
use super::model::*;
use crate::protocol::HubFrame;

/// An unanswered ask gets one reminder after this long.
pub const ASK_REMIND_MS: i64 = 15 * 60_000;
/// An unanswered ask expires after this long.
pub const ASK_EXPIRE_MS: i64 = 60 * 60_000;
/// How long an unaccepted delivery is remembered for replay.
const PENDING_KEEP_MS: i64 = 60 * 60_000;
/// A permission request expires (and is denied) after this long, since the agent is blocked on it.
pub const PERM_EXPIRE_MS: i64 = 15 * 60_000;

impl HubCore {
    /// Advances time. Returns the reminders, expiries and messages to agents that are due.
    pub fn tick(&mut self, now: i64) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.grants.retain(|g| g.expires_at > now);
        // A delivery nobody accepted for an hour is forgotten rather than kept forever.
        self.pending.retain(|_, p| now - p.at < PENDING_KEEP_MS);
        // Informing-only messages that waited long enough are delivered now.
        let waiting: Vec<String> = self
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for id in waiting {
            self.flush(&id, now, &mut fx);
        }
        let projects: Vec<String> = self.asks.keys().cloned().collect();
        for project in projects {
            let mut tell: Vec<(String, String)> = Vec::new();
            for ask in self
                .asks
                .get_mut(&project)
                .into_iter()
                .flatten()
                .filter(|a| a.state == AskState::Open)
            {
                let age = now - ask.opened;
                if age >= ASK_EXPIRE_MS {
                    ask.state = AskState::Expired;
                    tell.push((
                        ask.agent_id.clone(),
                        format!(
                            "Q{} expired. Proceed with your best assumption or stop.",
                            ask.qn
                        ),
                    ));
                    fx.push(Effect::Chat(Chat::Resolved {
                        project: project.clone(),
                        label: format!("Q{}", ask.qn),
                        how: "expired".into(),
                    }));
                } else if age >= ASK_REMIND_MS && !ask.reminded {
                    ask.reminded = true;
                    Self::notice(
                        &project,
                        format!("Q{} is still waiting for an answer.", ask.qn),
                        true,
                        &mut fx,
                    );
                }
            }
            for (agent_id, text) in tell {
                self.tell_id(&agent_id, text, now, &mut fx);
            }
        }
        let projects: Vec<String> = self.perms.keys().cloned().collect();
        for project in projects {
            let mut denied: Vec<PermRequest> = Vec::new();
            for p in self
                .perms
                .get_mut(&project)
                .into_iter()
                .flatten()
                .filter(|p| p.state == PermState::Open && now - p.opened >= PERM_EXPIRE_MS)
            {
                p.state = PermState::Expired;
                denied.push(p.clone());
            }
            for p in denied {
                if let Some(a) = self.agents.get(&p.agent_id).cloned() {
                    self.send_to(
                        &a,
                        HubFrame::Decision {
                            agent_id: p.agent_id.clone(),
                            perm_id: p.id.clone(),
                            allow: false,
                        },
                        &mut fx,
                    );
                }
                fx.push(Effect::Chat(Chat::Resolved {
                    project: project.clone(),
                    label: format!("P{}", p.pn),
                    how: "expired, denied".into(),
                }));
            }
        }
        fx
    }
}
