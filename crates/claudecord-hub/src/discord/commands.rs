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
    let agent = kind == STRING && matches!(name, "agent" | "from" | "command");
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
            "timer",
            "Stop every agent here after a set time (owner)",
            vec![
                sub(
                    "set",
                    "Stop all agents after a while, e.g. 90m, 2h, 1h30m",
                    vec![
                        opt(
                            "after",
                            STRING,
                            "How long from now: 90m, 2h, 1h30m, 1d",
                            true,
                        ),
                        opt("action", STRING, "stop (default) or pause", false),
                    ],
                ),
                sub("off", "Cancel the timer", vec![]),
                sub("status", "Show when the timer goes off", vec![]),
            ],
        ),
        cmd(
            "move",
            "Move an agent to another project (owner of both)",
            vec![
                opt("agent", STRING, "Which agent", true),
                opt("project", STRING, "The project to move it to", true),
            ],
        ),
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
            "tasks",
            "List tasks in this project with status and assignees",
            vec![opt(
                "agent",
                STRING,
                "Filter by agent (default: all)",
                false,
            )],
        ),
        cmd(
            "report",
            "Request a screen and activity report from agents",
            vec![opt(
                "agent",
                STRING,
                "Which agent, or 'all' (default: all)",
                false,
            )],
        ),
        cmd(
            "screen",
            "Show what an agent's terminal shows now (or 'report')",
            vec![opt(
                "agent",
                STRING,
                "Which agent, or 'report' / 'all' (default: the lead)",
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
                opt("command", STRING, "A saved startup command", false),
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

/// The longest a stop timer may be set for.
const TIMER_MAX_MS: i64 = 7 * 24 * 3_600_000;

/// How long a person meant by `90m`, `2h`, `1h30m`, `1d` or a bare number of minutes, in milliseconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    let (mut total, mut digits) = (0i64, String::new());
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit: i64 = match c {
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' => 60_000,
            _ => return None,
        };
        total = total.checked_add(digits.parse::<i64>().ok()?.checked_mul(unit)?)?;
        digits.clear();
    }
    if !digits.is_empty() {
        // No unit on the last number: minutes.
        total = total.checked_add(digits.parse::<i64>().ok()?.checked_mul(60_000)?)?;
    }
    (total > 0).then_some(total)
}

/// `/timer set|off|status`.
fn timer(
    core: &mut HubCore,
    sub: Option<&str>,
    o: &Opts,
    by: &Human,
    project: &str,
    now: i64,
) -> (String, Vec<Effect>) {
    let fail = |d: Denied| (denied_text(&d), vec![]);
    match sub {
        Some("set") => {
            let Some(ms) = o.get("after").and_then(|t| parse_duration(t)) else {
                return (
                    "I could not read that time. Try 90m, 2h, 1h30m or 1d.".into(),
                    vec![],
                );
            };
            if !(60_000..=TIMER_MAX_MS).contains(&ms) {
                return (
                    "The timer must be between 1 minute and 7 days.".into(),
                    vec![],
                );
            }
            let pause = match o.get("action").map(|a| a.trim().to_lowercase()).as_deref() {
                None | Some("") | Some("stop") => false,
                Some("pause") => true,
                Some(_) => return ("The action is `stop` or `pause`.".into(), vec![]),
            };
            match core.set_timer(by, project, now + ms, pause, now) {
                Ok(fx) => (
                    format!(
                        "All agents here will be {} <t:{}:R> (<t:{}:t>).",
                        if pause { "paused" } else { "stopped" },
                        (now + ms) / 1000,
                        (now + ms) / 1000
                    ),
                    fx,
                ),
                Err(d) => fail(d),
            }
        }
        Some("off") => match core.clear_timer(by, project, now) {
            Ok((true, fx)) => ("The timer is cancelled.".into(), fx),
            Ok((false, _)) => ("There is no timer set here.".into(), vec![]),
            Err(d) => fail(d),
        },
        _ => (
            match core.timer_of(project) {
                Some(t) => format!(
                    "All agents here will be {} <t:{}:R>, as set by {}.",
                    if t.pause { "paused" } else { "stopped" },
                    t.at / 1000,
                    t.by
                ),
                None => "There is no timer set here.".into(),
            },
            vec![],
        ),
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
        "timer" => timer(core, get("sub"), o, by, project, now),
        "tasks" => {
            let tasks = core.tasks_of(project);
            let filter = get("agent");
            let list: Vec<String> = tasks
                .iter()
                .filter(|t| {
                    if let Some(f) = filter {
                        let to = core
                            .agent(&t.to_agent)
                            .map(|a| a.name.as_str())
                            .unwrap_or("");
                        let from = core
                            .agent(&t.from_agent)
                            .map(|a| a.name.as_str())
                            .unwrap_or("");
                        to.eq_ignore_ascii_case(f) || from.eq_ignore_ascii_case(f)
                    } else {
                        true
                    }
                })
                .map(|t| {
                    let state_lbl = match t.state {
                        crate::hub::TaskState::Assigned => "ASSIGNED",
                        crate::hub::TaskState::Accepted => "IN PROGRESS",
                        crate::hub::TaskState::Done => "DONE",
                    };
                    let to_name = core
                        .agent(&t.to_agent)
                        .map_or_else(|| t.to_agent.clone(), |a| a.name.clone());
                    let from_name = core
                        .agent(&t.from_agent)
                        .map_or_else(|| t.from_agent.clone(), |a| a.name.clone());
                    let sum = t
                        .summary
                        .as_deref()
                        .map(|s| format!(" (Summary: {s})"))
                        .unwrap_or_default();
                    format!(
                        "• **[{}]** `[{state_lbl}]` @{from_name} ➜ @{to_name}: {}{sum}",
                        t.id, t.text
                    )
                })
                .collect();
            (
                if list.is_empty() {
                    "No tasks in this project.".into()
                } else {
                    list.join("\n")
                },
                vec![],
            )
        }
        "report" => {
            let agents: Vec<crate::hub::AgentRow> = core
                .agents_of_project(project)
                .into_iter()
                .cloned()
                .collect();
            if agents.is_empty() {
                ("No agents in this project.".into(), vec![])
            } else {
                let mut fx = Vec::new();
                let mut lines = vec![format!("**Screen Report for {project}**:")];
                for a in &agents {
                    let status = status_word(core.status_of(&a.agent_id));
                    let role = if a.is_lead { " (lead)" } else { "" };
                    lines.push(format!(
                        "• **{}{}** `[{status}]` on `{}`",
                        a.name, role, a.node_name
                    ));
                    match core.screen(by, project, &a.name) {
                        Ok(f) if !f.is_empty() => {
                            fx.extend(f);
                        }
                        _ => {
                            lines.push("  ↳ machine not connected".into());
                        }
                    }
                }
                (lines.join("\n"), fx)
            }
        }
        "screen" => {
            let target = get("agent");
            if target == Some("report") || target == Some("all") {
                let agents: Vec<crate::hub::AgentRow> = core
                    .agents_of_project(project)
                    .into_iter()
                    .cloned()
                    .collect();
                if agents.is_empty() {
                    return ("No agents in this project.".into(), vec![]);
                }
                let mut fx = Vec::new();
                let mut lines = vec![format!("**Screen Report for {project}**:")];
                for a in &agents {
                    let status = status_word(core.status_of(&a.agent_id));
                    let role = if a.is_lead { " (lead)" } else { "" };
                    lines.push(format!(
                        "• **{}{}** `[{status}]` on `{}`",
                        a.name, role, a.node_name
                    ));
                    match core.screen(by, project, &a.name) {
                        Ok(f) if !f.is_empty() => {
                            fx.extend(f);
                        }
                        _ => {
                            lines.push("  ↳ machine not connected".into());
                        }
                    }
                }
                return (lines.join("\n"), fx);
            }
            let name = match target {
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
        "move" => {
            let (name, to) = (get("agent").unwrap_or(""), get("project").unwrap_or(""));
            if !crate::protocol::is_slug(to) {
                return (
                    "A project name is letters, digits, dots, dashes and underscores.".into(),
                    vec![],
                );
            }
            match core.move_agent(by, project, name, to, now) {
                Ok((row, fx)) => (format!("{} moved to {to}.", row.name), fx),
                Err(crate::hub::controls::MoveError::Denied(d)) => fail(d),
                Err(crate::hub::controls::MoveError::Refused(why)) => (why, vec![]),
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
            // A saved command was resolved by the bridge: the program it starts is what the agent is read as.
            let adapter = match get("command_program")
                .or_else(|| get("adapter"))
                .unwrap_or("claude")
            {
                "agy" => AdapterId::Agy,
                "codex" => AdapterId::Codex,
                _ => AdapterId::Claude,
            };
            let extra = crate::hub::controls::SpawnExtra {
                command: get("command_line").map(String::from),
                pick: None,
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
            match core.spawn_auto_with(by, project, spec, get("label"), extra) {
                Ok((Some(node), fx)) => (format!("Asked {node} to start {n}."), fx),
                Ok((None, _)) => ("No machine has room (or the right label).".into(), vec![]),
                Err(d) => fail(d),
            }
        }
        _ => ("Unknown command.".into(), vec![]),
    }
}
