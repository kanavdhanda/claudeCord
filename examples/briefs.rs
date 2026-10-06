//! Prints the exact standing instructions an agent receives, as JSON. The cost benchmark reads this, so it always
//! measures the real text and never a copy that could drift from the code.

use claudecord::device::daemon::RULES;
use claudecord::hub::AgentRow;
use claudecord::hub::briefs;
use claudecord::protocol::AdapterId;

fn row(name: &str) -> AgentRow {
    AgentRow {
        agent_id: format!("demo/{name}"),
        name: name.into(),
        project: "demo".into(),
        node_name: "mac".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
        is_lead: false,
    }
}

fn main() {
    let (lead, a, b) = (row("otter"), row("heron"), row("wren"));
    let out = serde_json::json!({
        "lead_brief": briefs::lead(&lead, &[&a, &b]),
        "worker_brief": briefs::worker(&a, &lead, &[]),
        "rules": RULES,
    });
    println!("{out}");
}
