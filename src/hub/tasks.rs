//! Tasks: what the lead hands to its peers and how they are tracked. A task is Assigned, then Accepted when the peer
//! actually starts on it, then Done. Only the lead assigns. The lead hears about completions, and when every task is
//! done it is told to send one final report.

use super::core::{HubCore, Queued};
use super::effects::{Chat, Effect, Persist};
use super::model::*;

impl HubCore {
    /// Updates a task's state and keeps a copy of it.
    pub(super) fn set_task(
        &mut self,
        project: &str,
        id: &str,
        state: TaskState,
        summary: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(t) = self
            .tasks
            .get_mut(project)
            .and_then(|v| v.iter_mut().find(|t| t.id.eq_ignore_ascii_case(id)))
        else {
            return;
        };
        t.state = state;
        t.updated = now;
        if summary.is_some() {
            t.summary = summary;
        }
        let (to, cycle) = (t.to_agent.clone(), (now - t.created) as f64);
        let to_name = self
            .agents
            .get(&to)
            .map_or(to.rsplit('/').next().unwrap_or("").to_string(), |a| {
                a.name.clone()
            });
        Self::event(
            project,
            "task",
            &to_name,
            &format!("{state:?}").to_lowercase(),
            cycle,
            now,
            fx,
        );
        fx.push(Effect::Persist(Persist::History {
            project: project.into(),
            thread: None,
            from: "system".into(),
            kind: "task",
            text: format!("{} {:?}", t.id, t.state),
            at: now,
        }));
    }

    /// Queues a line from the system for one agent and releases its queue. The line is worth a turn by itself.
    pub(super) fn tell(&mut self, a: &AgentRow, text: String, now: i64, fx: &mut Vec<Effect>) {
        self.tell_with(a, text, true, now, fx);
    }

    /// Like `tell`, but with a choice: a line that only informs (`wake` false) rides along with the next line that
    /// needs a turn, instead of causing one.
    pub(super) fn tell_with(
        &mut self,
        a: &AgentRow,
        text: String,
        wake: bool,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        self.enqueue(
            &a.agent_id,
            Queued {
                from: "system".into(),
                text,
                thread: None,
                reference: None,
                task_id: None,
                handoff: None,
                wake,
                at: now,
                say: None,
            },
        );
        self.flush(&a.agent_id, now, fx);
    }

    /// Same as `tell`, for when only the agent's id is at hand.
    pub(super) fn tell_id(&mut self, agent_id: &str, text: String, now: i64, fx: &mut Vec<Effect>) {
        if let Some(a) = self.agents.get(agent_id).cloned() {
            self.tell(&a, text, now, fx);
        }
    }

    /// The lead gives a task to a peer. Anyone else who tries is told only the lead assigns. The task text is scrubbed.
    pub(super) fn assign(
        &mut self,
        from_id: &str,
        to_name: &str,
        task: &str,
        thread: Option<String>,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(from) = self.agents.get(from_id).cloned() else {
            return;
        };
        if !from.is_lead {
            self.tell(&from, "Only the lead assigns tasks.".into(), now, fx);
            return;
        }
        let to = self
            .find_by_name(&from.project, to_name)
            .filter(|t| t.agent_id != from.agent_id)
            .cloned();
        let Some(to) = to else {
            let peers: Vec<&str> = self
                .agents_of_project(&from.project)
                .into_iter()
                .filter(|p| p.agent_id != from.agent_id)
                .map(|p| p.name.as_str())
                .collect();
            let list = if peers.is_empty() {
                "none".to_string()
            } else {
                peers.join(", ")
            };
            self.tell(
                &from,
                format!("Cannot assign to {to_name}. Peers: {list}."),
                now,
                fx,
            );
            return;
        };
        let task = self.scrub(&from, task, now, fx);
        let num = self
            .tasks
            .get(&from.project)
            .and_then(|v| v.iter().map(|t| t.num).max())
            .unwrap_or(0)
            + 1;
        let row = TaskRow {
            id: format!("T{num}"),
            project: from.project.clone(),
            num,
            from_agent: from.agent_id.clone(),
            to_agent: to.agent_id.clone(),
            text: task.clone(),
            state: TaskState::Assigned,
            summary: None,
            created: now,
            updated: now,
        };
        // Every task gets its own thread by itself, so the main chat keeps only results and questions.
        let thread = thread.or_else(|| Some(task_thread(&row)));
        self.tasks
            .entry(from.project.clone())
            .or_default()
            .push(row.clone());
        self.metrics.inc("task_assigned", 1.0, now);
        Self::event(&from.project, "task", &to.name, "assigned", 0.0, now, fx);
        fx.push(Effect::Persist(Persist::History {
            project: from.project.clone(),
            thread: thread.clone(),
            from: from.name.clone(),
            kind: "task",
            text: format!("{} to {}: {task}", row.id, to.name),
            at: now,
        }));
        self.enqueue(
            &to.agent_id,
            Queued {
                from: from.name.clone(),
                text: format!("{}: {task}", row.id),
                thread: thread.clone(),
                reference: None,
                task_id: Some(row.id.clone()),
                handoff: None,
                wake: true,
                at: now,
                say: None,
            },
        );
        self.flush(&to.agent_id, now, fx);
        fx.push(Effect::Chat(Chat::Post {
            project: from.project.clone(),
            agent: from.clone(),
            text: format!("@{} {}: {task}", to.name, row.id),
            thread,
        }));
        if !self.conns.contains_key(&to.node_name) {
            self.tell(
                &from,
                format!("{} is offline. {} waits for them.", to.name, row.id),
                now,
                fx,
            );
        }
    }

    /// A peer reports a task finished. Only the assignee can. The lead hears about it, and hears when all are done.
    pub(super) fn task_done(
        &mut self,
        agent_id: &str,
        task_id: &str,
        summary: &str,
        now: i64,
        fx: &mut Vec<Effect>,
    ) {
        let Some(a) = self.agents.get(agent_id).cloned() else {
            return;
        };
        let task = self
            .tasks
            .get(&a.project)
            .and_then(|v| v.iter().find(|t| t.id.eq_ignore_ascii_case(task_id)))
            .cloned();
        let Some(t) = task.filter(|t| t.to_agent == a.agent_id) else {
            self.tell(&a, format!("{task_id} is not assigned to you."), now, fx);
            return;
        };
        // A fresh session may finish a task its predecessor already finished. Say so once, do not tell the lead twice.
        if t.state == TaskState::Done {
            self.tell(&a, format!("{} is already done.", t.id), now, fx);
            return;
        }
        let summary = self.scrub(&a, summary, now, fx);
        self.set_task(
            &a.project,
            &t.id,
            TaskState::Done,
            Some(summary.clone()),
            now,
            fx,
        );
        self.metrics.task_finished((now - t.created) as f64, now);
        fx.push(Effect::Chat(Chat::Post {
            project: a.project.clone(),
            agent: a.clone(),
            text: format!("Finished {}: {summary}", t.id),
            thread: None,
        }));
        let Some(lead) = self.agents.get(&t.from_agent).cloned() else {
            return;
        };
        // The lead only needs a turn for this when it was the last task. Earlier completions ride along.
        let all_done = self
            .tasks_of(&a.project)
            .iter()
            .all(|x| x.state == TaskState::Done);
        self.tell_with(
            &lead,
            format!("{} done {}: {summary}", a.name, t.id),
            all_done,
            now,
            fx,
        );
        let all = self.tasks_of(&a.project);
        if !all.is_empty() && all_done {
            let n = all.len();
            self.tell(
                &lead,
                format!("All {n} task(s) done. Send the final report."),
                now,
                fx,
            );
            Self::notice(&a.project, format!("All {n} task(s) are done."), false, fx);
        }
    }
}

/// The chat thread of a task: its number and the start of its text, on one line (the chat caps thread names at 100 characters).
pub(super) fn task_thread(t: &TaskRow) -> String {
    let words: String = t.text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{} {}", t.id, words.chars().take(40).collect::<String>())
        .trim_end()
        .to_string()
}

impl HubCore {
    /// The thread of the newest task still open for an agent, so what it says while working goes there with no flag to remember.
    pub(super) fn open_task_thread(&self, agent_id: &str, project: &str) -> Option<String> {
        self.tasks
            .get(project)?
            .iter()
            .rev()
            .find(|t| t.to_agent == agent_id && t.state != TaskState::Done)
            .map(task_thread)
    }
}
