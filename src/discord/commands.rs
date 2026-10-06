//! The slash commands people use in Discord, and what each one does. The list Discord shows is `definitions`. `handle` takes
//! one command that was used, runs it against the hub core, and returns the short reply for the person. It touches no
//! network, so every command can be tested without Discord. The role checks live in the core, not here, so a command can
//! never do more than the person's role allows.

use crate::hub::{Denied, Effect, HubCore, Human, Role};
use crate::protocol::{AdapterId, AgentSpec, AgentStatus};
use serde_json::{Value, json};
use std::collections::HashMap;

/// The options given to a command, by name, as text. A user option carries the account id under its name and the display
/// name under `<name>_name`.
pub type Opts = HashMap<String, String>;

const STRING: u8 = 3;
const INTEGER: u8 = 4;
const USER: u8 = 6;

fn opt(name: &str, kind: u8, description: &str, required: bool) -> Value {
    // An agent's name is chosen from a list of the agents here (Discord asks the bridge for it as the person types).
    let agent = kind == STRING && matches!(name, "agent" | "from");
    json!({"name": name, "description": description, "type": kind, "required": required, "autocomplete": agent})
}

/// A sub-command: `/clear agent` and `/clear chat` are two of these under `/clear`, so which is meant is always chosen from the list.
fn sub(name: &str, description: &str, options: Vec<Value>) -> Value {
    json!({"name": name, "description": description, "type": 1, "options": options})
}

fn cmd(name: &str, description: &str, options: Vec<Value>) -> Value {
    json!({"name": name, "description": description, "type": 1, "options": options})
}

/// The commands as Discord wants them registered.
pub fn definitions() -> Value {
    // Short on purpose, and always under Discord's limits (a command name up to 32 characters, a description up to 100): one over the limit and
    // Discord refuses the whole list, so none of the commands show up.
    Value::Array(vec![
        cmd("agents", "List the agents here", vec![]),
        cmd("devices", "List the machines and who is connected", vec![]),
        cmd("status", "A summary of this project", vec![]),
        cmd(
            "pause",
            "Hold messages to an agent (default: all)",
            vec![opt("agent", STRING, "Which agent", false)],
        ),
        cmd(
            "resume",
            "Release held messages",
            vec![opt("agent", STRING, "Which agent", false)],
        ),
        cmd(
            "stop",
            "Stop one agent",
            vec![opt("agent", STRING, "Which agent", true)],
        ),
        cmd("killall", "Stop every agent here (owner)", vec![]),
        cmd(
            "clear",
            "Start over: one agent, or the whole chat",
            vec![
                sub(
                    "agent",
                    "Start one agent over, clean (default: the lead)",
                    vec![opt("agent", STRING, "Which agent", false)],
                ),
                sub(
                    "chat",
                    "New chat: clear the channel, restart all (owner)",
                    vec![],
                ),
            ],
        ),
        cmd(
            "screen",
            "Show what an agent's terminal shows now",
            vec![opt(
                "agent",
                STRING,
                "Which agent (default: the lead)",
                false,
            )],
        ),
        cmd(
            "lead",
            "Choose which agent leads (owner)",
            vec![opt("agent", STRING, "The new lead", true)],
        ),
        cmd(
            "btw",
            "A short aside to an agent",
            vec![
                opt("text", STRING, "What to say", true),
                opt("agent", STRING, "Which agent (default: the lead)", false),
            ],
        ),
        cmd(
            "grant",
            "Give an agent standing permission (owner)",
            vec![
                opt("agent", STRING, "Which agent (default: all)", false),
                opt("kind", STRING, "Such as edit or bash (default: any)", false),
                opt("minutes", INTEGER, "How long (default 60)", false),
            ],
        ),
        cmd("revoke", "End all standing permissions (owner)", vec![]),
        cmd(
            "role",
            "Set what a person may do (owner)",
            vec![
                opt("user", USER, "Who", true),
                opt("role", STRING, "viewer, operator, owner or none", true),
            ],
        ),
        cmd(
            "dump",
            "Ask agents to save their state",
            vec![opt("agent", STRING, "Which agent (default: all)", false)],
        ),
        cmd(
            "pickup",
            "Give a saved handoff to an agent",
            vec![
                opt("agent", STRING, "Who receives it", true),
                opt("from", STRING, "Whose handoff (default: its own)", false),
            ],
        ),
        cmd(
            "raw",
            "Type exact text into an agent (owner)",
            vec![
                opt("agent", STRING, "Which agent", true),
                opt("text", STRING, "The exact text, such as /compact", true),
            ],
        ),
        cmd(
            "spawn",
            "Start another agent (owner)",
            vec![
                opt("name", STRING, "Name for it", true),
                opt("adapter", STRING, "claude, agy or codex", false),
                opt("model", STRING, "Model", false),
                opt("role", STRING, "Its job", false),
                opt("label", STRING, "Kind of machine, such as gpu", false),
            ],
        ),
    ])
}

/// A denial in words a person can act on.
pub fn denied_text(d: &Denied) -> String {
    match d {
        Denied::Unlisted => "You are not on this project's list.".into(),
        Denied::NeedsRole(r) => format!("That needs the {} role.", r.name()),
        Denied::NotFound => "Nothing by that name here.".into(),
        Denied::AlreadyDone { by } => format!("Already settled by {by}."),
        Denied::NotAllowedFor => "That is not allowed.".into(),
    }
}

fn status_word(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Starting => "starting",
        AgentStatus::Idle => "idle",
        AgentStatus::Thinking => "thinking",
        AgentStatus::Executing => "working",
        AgentStatus::WaitingInput => "waiting for an answer",
        AgentStatus::Paused => "paused",
        AgentStatus::Limited => "at a usage limit",
        AgentStatus::Offline => "offline",
    }
}

/// Runs one slash command and returns the reply to show, plus anything the core wants carried out.
pub fn handle(
    core: &mut HubCore,
    name: &str,
    o: &Opts,
    by: &Human,
    project: &str,
    now: i64,
) -> (String, Vec<Effect>) {
    let get = |k: &str| o.get(k).map(String::as_str);
    let fail = |d: Denied| (denied_text(&d), vec![]);
    match name {
        "agents" => {
            let list: Vec<String> = core
                .agents_of_project(project)
                .iter()
                .map(|a| {
                    format!(
                        "{}{} ({}): {}",
                        a.name,
                        if a.is_lead { " (lead)" } else { "" },
                        a.node_name,
                        status_word(core.status_of(&a.agent_id))
                    )
                })
                .collect();
            (
                if list.is_empty() {
                    "No agents here yet.".into()
                } else {
                    list.join("\n")
                },
                vec![],
            )
        }
        "devices" => {
            let list: Vec<String> = core
                .devices()
                .iter()
                .map(|d| {
                    format!(
                        "{}: {}, {} agent(s){}",
                        d.node,
                        if d.connected {
                            "connected"
                        } else {
                            "not connected"
                        },
                        d.agents.len(),
                        d.max_agents.map_or(String::new(), |m| format!(" of {m}"))
                    )
                })
                .collect();
            (
                if list.is_empty() {
                    "No machines yet.".into()
                } else {
                    list.join("\n")
                },
                vec![],
            )
        }
        "status" => {
            let asks = core
                .asks_of(project)
                .iter()
                .filter(|a| a.state == crate::hub::AskState::Open)
                .count();
            let tasks = core.tasks_of(project);
            let done = tasks
                .iter()
                .filter(|t| t.state == crate::hub::TaskState::Done)
                .count();
            (
                format!(
                    "{} agent(s), {asks} open question(s), {done} of {} task(s) done.",
                    core.agents_of_project(project).len(),
                    tasks.len()
                ),
                vec![],
            )
        }
        "pause" | "resume" => match core.hold(by, name == "pause", project, get("agent"), now) {
            Ok((n, fx)) => (
                format!(
                    "{} {n} agent(s).",
                    if name == "pause" { "Paused" } else { "Resumed" }
                ),
                fx,
            ),
            Err(d) => fail(d),
        },
        "stop" => match core.stop(by, project, get("agent").unwrap_or(""), now) {
            Ok((true, fx)) => ("Stopping.".into(), fx),
            Ok((false, _)) => ("That agent's machine is not connected.".into(), vec![]),
            Err(d) => fail(d),
        },
        "killall" => match core.kill_all(by, Some(project), now) {
            Ok((n, fx)) => (format!("Asked {n} agent(s) to quit."), fx),
            Err(d) => fail(d),
        },
        "screen" => {
            let name = match get("agent") {
                Some(n) => Some(n.to_string()),
                None => core
                    .agents_of_project(project)
                    .iter()
                    .find(|a| a.is_lead)
                    .map(|a| a.name.clone()),
            };
            match name {
                Some(n) => match core.screen(by, project, &n) {
                    Ok(fx) if fx.is_empty() => (
                        format!("{n}'s machine is not connected, so its terminal cannot be read."),
                        fx,
                    ),
                    // No words: the picture is the answer (the bridge answers the command without leaving a message behind).
                    Ok(fx) => (String::new(), fx),
                    Err(d) => fail(d),
                },
                None => ("There is no agent in this project.".into(), vec![]),
            }
        }
        "lead" => {
            let name = get("agent").unwrap_or("");
            match core.find_by_name(project, name).map(|a| a.agent_id.clone()) {
                Some(id) => match core.set_lead(by, project, &id) {
                    Ok((lead, fx)) => (format!("{} now leads.", lead.name), fx),
                    Err(d) => fail(d),
                },
                None => ("No agent with that name here.".into(), vec![]),
            }
        }
        "clear" => {
            if get("sub") == Some("chat") {
                // The whole chat: the channel is cleared and every agent starts over. Owner only.
                match core.clear_chat(by, project, now) {
                    Ok((n, fx)) => (
                        format!("Starting a fresh chat: {n} agent(s) start over."),
                        fx,
                    ),
                    Err(d) => fail(d),
                }
            } else {
                // One agent: the one named, or the lead. The channel is left alone.
                let name = match get("agent") {
                    Some(n) => Some(n.to_string()),
                    None => core
                        .agents_of_project(project)
                        .iter()
                        .find(|a| a.is_lead)
                        .map(|a| a.name.clone()),
                };
                match name {
                    Some(n) => match core.clear_agent(by, project, &n, now) {
                        Ok(fx) => (format!("{n} is starting over."), fx),
                        Err(d) => fail(d),
                    },
                    None => ("There is no agent in this project to clear.".into(), vec![]),
                }
            }
        }
        "btw" => match core.btw(by, project, get("text").unwrap_or(""), get("agent"), now) {
            Ok((r, fx)) => (format!("Sent to {}.", r.targets.join(", ")), fx),
            Err(d) => fail(d),
        },
        "grant" => {
            let minutes = get("minutes")
                .and_then(|m| m.parse::<i64>().ok())
                .unwrap_or(60)
                .clamp(1, 24 * 60);
            match core.grant(
                by,
                project,
                get("agent"),
                get("kind"),
                Some(minutes * 60_000),
                now,
            ) {
                Ok(g) => (
                    format!(
                        "Granted {} for {} for {minutes} minute(s).",
                        g.kind.as_deref().unwrap_or("everything"),
                        get("agent").unwrap_or("all agents")
                    ),
                    vec![],
                ),
                Err(d) => fail(d),
            }
        }
        "revoke" => match core.revoke_grants(by, project) {
            Ok(n) => (format!("Ended {n} standing permission(s)."), vec![]),
            Err(d) => fail(d),
        },
        "role" => {
            let role = match get("role") {
                Some("viewer") => Some(Role::Viewer),
                Some("operator") => Some(Role::Operator),
                Some("owner") => Some(Role::Owner),
                Some("none") => None,
                _ => return ("Roles are viewer, operator, owner or none.".into(), vec![]),
            };
            let target = Human {
                id: get("user").unwrap_or("").into(),
                name: get("user_name").unwrap_or("someone").into(),
            };
            match core.set_role(by, project, &target, role) {
                Ok(()) => (
                    format!(
                        "{} is now {}.",
                        target.name,
                        role.map_or("removed", Role::name)
                    ),
                    vec![],
                ),
                Err(d) => fail(d),
            }
        }
        "dump" => match core.dump(by, project, get("agent"), now) {
            Ok((n, fx)) => (format!("Asked {n} agent(s) to save their state."), fx),
            Err(d) => fail(d),
        },
        "pickup" => {
            match core.pickup_for(by, project, get("agent").unwrap_or(""), get("from"), now) {
                Ok(fx) => ("Handoff given.".into(), fx),
                Err(d) => fail(d),
            }
        }
        "raw" => match core.raw_input(
            by,
            project,
            get("agent").unwrap_or(""),
            get("text").unwrap_or(""),
            now,
        ) {
            Ok((true, fx)) => ("Typed.".into(), fx),
            Ok((false, _)) => ("That agent's machine is not connected.".into(), vec![]),
            Err(d) => fail(d),
        },
        "spawn" => {
            let adapter = match get("adapter").unwrap_or("claude") {
                "agy" => AdapterId::Agy,
                "codex" => AdapterId::Codex,
                _ => AdapterId::Claude,
            };
            let n = get("name").unwrap_or("").to_ascii_lowercase();
            let n = n.as_str();
            if let Some(why) = crate::protocol::agent_name_problem(n) {
                return (format!("{}.", why[..1].to_uppercase() + &why[1..]), vec![]);
            }
            let spec = AgentSpec {
                agent_id: format!("{project}/{n}"),
                name: n.into(),
                project: project.into(),
                adapter,
                model: get("model").map(String::from),
                role: get("role").map(String::from),
            };
            match core.spawn_auto(by, project, spec, get("label")) {
                Ok((Some(node), fx)) => (format!("Asked {node} to start {n}."), fx),
                Ok((None, _)) => ("No machine has room (or the right label).".into(), vec![]),
                Err(d) => fail(d),
            }
        }
        _ => ("Unknown command.".into(), vec![]),
    }
}
