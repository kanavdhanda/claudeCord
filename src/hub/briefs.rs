//! The short standing instructions an agent gets once, with its first delivery. Every character here is paid for in
//! tokens on every agent, so these are tiny and never change from one call to the next (which keeps them cacheable).

use super::model::AgentRow;

/// Instructions for the agent that leads a project.
pub fn lead(a: &AgentRow, peers: &[&AgentRow]) -> String {
    if peers.is_empty() {
        return format!("You lead {}. Work alone until peers join.", a.project);
    }
    let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
    format!(
        "You lead {}. Peers: {}. Split work with assign. When all are done, send one report.",
        a.project,
        names.join(", ")
    )
}

/// Instructions for an agent that is not the lead.
pub fn worker(a: &AgentRow, lead: &AgentRow) -> String {
    format!(
        "{} leads {}. Do their tasks, then run done <id> <summary>.",
        lead.name, a.project
    )
}

/// One line telling an agent who else is in the project, sent only when the roster changed.
pub fn roster(a: &AgentRow, all: &[&AgentRow]) -> String {
    let names: Vec<&str> = all
        .iter()
        .filter(|p| p.agent_id != a.agent_id)
        .map(|p| p.name.as_str())
        .collect();
    format!("peers: {}", names.join(", "))
}
