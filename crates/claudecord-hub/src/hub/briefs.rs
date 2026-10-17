//! The short standing instructions an agent gets once, with its first delivery. Every character here is paid for in
//! tokens on every agent, so these are tiny and never change from one call to the next (which keeps them cacheable).

use super::model::AgentRow;

/// A name with its job when it has one: `heron (tester)`.
fn who(p: &AgentRow) -> String {
    match p.role.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => format!("{} ({r})", p.name),
        None => p.name.clone(),
    }
}

/// Instructions for the agent that leads a project.
pub fn lead(a: &AgentRow, peers: &[&AgentRow]) -> String {
    if peers.is_empty() {
        return format!("You lead {}. Work alone until peers join.", a.project);
    }
    let names: Vec<String> = peers.iter().map(|p| who(p)).collect();
    format!(
        "You lead {}. Peers: {}. Split work with assign. When all are done, send one report.",
        a.project,
        names.join(", ")
    )
}

/// Instructions for an agent that is not the lead. It is told about the other workers too, so it knows who it can @name.
pub fn worker(a: &AgentRow, lead: &AgentRow, others: &[&AgentRow]) -> String {
    // Not "do their tasks": people write to a worker directly too, and an agent told it must be doing assigned work answers a plain message with
    // "I have no task". So: who leads, that people may write to it, and how a task ends.
    let mut s = format!(
        "{} leads {}. People may also write to you directly: answer them with say. A task from {} ends with done <id> <summary>.",
        who(lead),
        a.project,
        lead.name
    );
    if !others.is_empty() {
        let names: Vec<String> = others.iter().map(|p| who(p)).collect();
        s.push_str(&format!(
            " Also here: {}. @name one to need a reply.",
            names.join(", ")
        ));
    }
    s
}

/// One line telling an agent who else is in the project, sent only when the roster changed.
pub fn roster(a: &AgentRow, all: &[&AgentRow]) -> String {
    let names: Vec<String> = all
        .iter()
        .filter(|p| p.agent_id != a.agent_id)
        .map(|p| who(p))
        .collect();
    format!("peers: {}", names.join(", "))
}

/// The answer to `claudecord team`: everyone in the project with their job, who leads and whether they can be reached now.
pub fn team(all: &[(&AgentRow, &str, bool)]) -> String {
    let rows: Vec<String> = all
        .iter()
        .map(|(p, status, online)| {
            let lead = if p.is_lead { ", lead" } else { "" };
            let away = if *online { "" } else { ", offline" };
            format!("{}{lead}, {status}{away}", who(p))
        })
        .collect();
    format!("team: {}", rows.join("; "))
}
