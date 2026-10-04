//! The Obsidian export. It turns history rows into a vault: plain Markdown files with `[[wikilinks]]`, which Obsidian
//! reads as a graph. Nothing here talks to Obsidian or needs it installed; the files are ordinary notes, and the vault can
//! be opened (or synced, or put in git) like any other folder.
//!
//! Layout of the vault:
//! - `Home.md`                              every project
//! - `<project>/index.md`                   threads, people and tasks of one project
//! - `<project>/Chat/<thread>/index.md`     one note per thread, linking its days
//! - `<project>/Chat/<thread>/<date>.md`    the messages of one thread on one day, in order
//! - `<project>/Tasks/T1.md`                one note per task, linking who gave it and who did it
//! - `<project>/Asks/Q1.md`                 one note per question: who asked, the question, who answered, the answer
//! - `<project>/Permissions/P1.md`          one note per permission request: what was wanted and who decided
//! - `People/<name>.md`                     one note per person or agent, shared across projects
//!
//! Links go from every message to the people it names, to the tasks it mentions, and up to its thread, so the graph
//! shows clusters per thread tied together by the people and tasks they share.
//!
//! The functions here are pure (rows in, files out) so they are easy to test. `write_vault` writes only the files whose
//! content changed, so running it again and again is cheap and does not disturb Obsidian.

use crate::agents::text::safe_name;
use crate::store::HistoryRow;
use crate::store::bucket::stamp;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A file to write: its path inside the vault, and its text.
pub type NoteFile = (String, String);

/// A name made safe to use as a file name inside the vault.
fn file_name(s: &str) -> String {
    let n = safe_name(s);
    if n.is_empty() { "_".into() } else { n }
}

/// The person (or agent) a header like `kd (owner)` or `sam (btw)` belongs to: the name without the bracketed part.
fn person_of(from: &str) -> String {
    file_name(from.split(" (").next().unwrap_or(from).trim())
}

/// `YYYY-MM-DD` and `HH:MM` for a time in milliseconds since the epoch (UTC).
fn when(ms: i64) -> (String, String) {
    let s = stamp(ms.div_euclid(1000));
    (
        format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]),
        format!("{}:{}", &s[9..11], &s[11..13]),
    )
}

/// Ids such as `T3` (or `Q3`, `P3` for the given letter) in a text, in order.
fn ids_with(text: &str, letter: u8) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let word_start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if b[i] == letter && word_start {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j == b.len() || !b[j].is_ascii_alphanumeric()) {
                out.push(text[i..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Task ids such as `T3` in a text, in order.
fn task_ids(text: &str) -> Vec<String> {
    ids_with(text, b'T')
}

/// What a text points at: people by `@name`, tasks `T1`, questions `Q1` and permission requests `P1`.
#[derive(Default)]
struct Links {
    people: BTreeSet<String>,
    tasks: BTreeSet<String>,
    asks: BTreeSet<String>,
    perms: BTreeSet<String>,
}

/// Replaces `@name` with a link to that person and `T3` with a link to that task. Returns the new text, the people named
/// and the tasks mentioned.
fn link_text(project: &str, text: &str) -> (String, Links) {
    let mut links = Links::default();
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev_word = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if c == '@' && !prev_word {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_alphanumeric() || matches!(chars[j], '-' | '_' | '.'))
            {
                j += 1;
            }
            let name: String = chars[i + 1..j]
                .iter()
                .collect::<String>()
                .trim_end_matches('.')
                .to_string();
            if !name.is_empty() {
                let n = file_name(&name);
                out.push_str(&format!("[[People/{n}|@{name}]]"));
                links.people.insert(n);
                i += 1 + name.chars().count();
                continue;
            }
        }
        if matches!(c, 'T' | 'Q' | 'P') && !prev_word {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j == chars.len() || !chars[j].is_alphanumeric()) {
                let id: String = chars[i..j].iter().collect();
                let (folder, set) = match c {
                    'T' => ("Tasks", &mut links.tasks),
                    'Q' => ("Asks", &mut links.asks),
                    _ => ("Permissions", &mut links.perms),
                };
                out.push_str(&format!("[[{}/{folder}/{id}|{id}]]", file_name(project)));
                set.insert(id);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    (out, links)
}

/// One message as a Markdown list item. Extra lines are indented so they stay inside the item.
fn line(project: &str, r: &HistoryRow) -> String {
    let (_, time) = when(r.at);
    let (text, _) = link_text(project, &r.text);
    let who = person_of(&r.from);
    let sender = if r.from == "system" {
        "system".to_string()
    } else {
        format!("[[People/{who}|{}]]", r.from.replace('|', "/"))
    };
    let body = text.replace('\n', "\n  ");
    format!("- {time} **{sender}** ({}): {body}\n", r.kind)
}

/// Builds the whole vault for the given rows (any number of projects).
pub fn render(rows: &[HistoryRow]) -> Vec<NoteFile> {
    // Group by project, then thread, then day.
    let mut by_project: BTreeMap<&str, Vec<&HistoryRow>> = BTreeMap::new();
    for r in rows {
        by_project.entry(&r.project).or_default().push(r);
    }
    let mut files: Vec<NoteFile> = Vec::new();
    // People appear in several projects, so their notes are built from all of it.
    let mut people_in: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
    for (project, prows) in &by_project {
        let proj = file_name(project);
        let mut threads: BTreeMap<String, BTreeMap<String, Vec<&HistoryRow>>> = BTreeMap::new();
        let mut tasks: BTreeMap<String, TaskNote> = BTreeMap::new();
        let mut asks: BTreeMap<String, AskNote> = BTreeMap::new();
        let mut perms: BTreeMap<String, PermNote> = BTreeMap::new();
        for r in prows {
            let thread = r.thread.as_deref().map_or("general".to_string(), file_name);
            threads
                .entry(thread)
                .or_default()
                .entry(when(r.at).0)
                .or_default()
                .push(r);
            let who = person_of(&r.from);
            if r.from != "system" {
                let kind = if r.kind == "human" { "human" } else { "agent" };
                let e = people_in
                    .entry(who.clone())
                    .or_insert((kind.to_string(), BTreeSet::new()));
                e.1.insert(proj.clone());
            }
            // Task rows: "T1 to heron: text" (assigned) and "T1 Done" (state change).
            if r.kind == "task"
                && let Some((id, rest)) = r.text.split_once(' ')
            {
                let t = tasks.entry(id.to_string()).or_default();
                if let Some((to, desc)) = rest.strip_prefix("to ").and_then(|x| x.split_once(": "))
                {
                    t.assignee = Some(file_name(to));
                    t.lead = Some(who.clone());
                    t.text = desc.to_string();
                    t.status = "assigned".into();
                } else {
                    t.status = rest.trim().to_lowercase();
                }
            }
            for id in task_ids(&r.text) {
                tasks.entry(id).or_default();
            }
            // Questions and their answers: "Q1: which db?" from the asker, "Q1: postgres" from whoever answered.
            let day = when(r.at).0;
            match r.kind.as_str() {
                "ask" => {
                    if let Some((id, q)) = r.text.split_once(": ") {
                        let n = asks.entry(id.to_string()).or_default();
                        n.asker = Some(who.clone());
                        n.question = q.to_string();
                        n.thread = r.thread.as_deref().map_or("general".to_string(), file_name);
                        n.day = day;
                    }
                }
                "answer" => {
                    if let Some((id, a)) = r.text.split_once(": ") {
                        let n = asks.entry(id.to_string()).or_default();
                        n.answerer = Some((who.clone(), r.from.clone()));
                        n.answer = a.to_string();
                    }
                }
                "permission" => {
                    if let Some((id, rest)) = r.text.split_once(' ')
                        && let Some((kind, action)) = rest.split_once(": ")
                    {
                        let n = perms.entry(id.to_string()).or_default();
                        n.agent = Some(who.clone());
                        n.kind = kind.to_string();
                        n.action = action.to_string();
                        n.thread = r.thread.as_deref().map_or("general".to_string(), file_name);
                        n.day = day;
                    }
                }
                "decision" => {
                    if let Some((id, how)) = r.text.split_once(' ') {
                        let n = perms.entry(id.to_string()).or_default();
                        n.decision = how.to_string();
                        n.by = Some((who.clone(), r.from.clone()));
                    }
                }
                _ => {}
            }
            // A question or request mentioned anywhere gets a note, even if its own rows are not in this export.
            let (_, links) = link_text(project, &r.text);
            for id in links.asks {
                asks.entry(id).or_default();
            }
            for id in links.perms {
                perms.entry(id).or_default();
            }
        }
        // Chat notes: one per thread and day, and one index per thread.
        let mut thread_list = Vec::new();
        for (thread, days) in &threads {
            let mut day_links = Vec::new();
            for (date, drows) in days {
                let mut body = String::new();
                for r in drows {
                    body.push_str(&line(project, r));
                }
                let up = format!("[[{proj}/Chat/{thread}/index|{thread}]]");
                files.push((
                    format!("{proj}/Chat/{thread}/{date}.md"),
                    format!("---\nproject: {project}\nthread: {thread}\ndate: {date}\ntags: [claudecord, chat]\n---\n# {project} / {thread} / {date}\n\nUp: {up}\n\n{body}"),
                ));
                day_links.push(format!(
                    "- [[{proj}/Chat/{thread}/{date}|{date}]] ({} messages)\n",
                    drows.len()
                ));
            }
            files.push((
                format!("{proj}/Chat/{thread}/index.md"),
                format!("---\nproject: {project}\nthread: {thread}\ntags: [claudecord, thread]\n---\n# {thread}\n\nProject: [[{proj}/index|{project}]]\n\n{}", day_links.concat()),
            ));
            thread_list.push(format!("- [[{proj}/Chat/{thread}/index|{thread}]]\n"));
        }
        // Task notes.
        let mut task_list = Vec::new();
        for (id, t) in &tasks {
            let who = |label: &str, v: &Option<String>| {
                v.as_ref()
                    .map(|n| format!("{label}: [[People/{n}|{n}]]\n"))
                    .unwrap_or_default()
            };
            files.push((
                format!("{proj}/Tasks/{id}.md"),
                format!(
                    "---\nproject: {project}\ntask: {id}\nstatus: {}\ntags: [claudecord, task]\n---\n# {id}\n\n{}{}Project: [[{proj}/index|{project}]]\n\n{}\n",
                    if t.status.is_empty() { "unknown" } else { &t.status },
                    who("Given by", &t.lead),
                    who("Assigned to", &t.assignee),
                    t.text
                ),
            ));
            task_list.push(format!(
                "- [[{proj}/Tasks/{id}|{id}]] ({})\n",
                if t.status.is_empty() {
                    "unknown"
                } else {
                    &t.status
                }
            ));
        }
        // Question notes: what was asked, who answered, and what the answer was.
        let mut ask_list = Vec::new();
        for (id, a) in &asks {
            let status = if a.answerer.is_some() {
                "answered"
            } else {
                "open"
            };
            let person = |p: &Option<String>| {
                p.as_ref()
                    .map(|n| format!("[[People/{n}|{n}]]"))
                    .unwrap_or_else(|| "unknown".into())
            };
            let quote = |t: &str| t.lines().map(|l| format!("> {l}\n")).collect::<String>();
            let where_ = if a.day.is_empty() {
                String::new()
            } else {
                format!(
                    " in [[{proj}/Chat/{}/{}|{} / {}]]",
                    a.thread, a.day, a.thread, a.day
                )
            };
            let answer = match &a.answerer {
                Some((n, label)) => format!(
                    "Answered by [[People/{n}|{}]]:\n\n{}",
                    label.replace('|', "/"),
                    quote(&a.answer)
                ),
                None => "Not answered yet.\n".to_string(),
            };
            files.push((
                format!("{proj}/Asks/{id}.md"),
                format!(
                    "---\nproject: {project}\nask: {id}\nstatus: {status}\nasked_by: {}\nanswered_by: {}\ntags: [claudecord, ask]\n---\n# {id}\n\nProject: [[{proj}/index|{project}]]\n\nAsked by {}{where_}:\n\n{}\n{answer}",
                    a.asker.clone().unwrap_or_default(),
                    a.answerer.as_ref().map(|x| x.0.clone()).unwrap_or_default(),
                    person(&a.asker),
                    quote(&a.question),
                ),
            ));
            ask_list.push(format!("- [[{proj}/Asks/{id}|{id}]] ({status})\n"));
        }
        // Permission notes: what an agent wanted, and who decided.
        let mut perm_list = Vec::new();
        for (id, p) in &perms {
            let status = if p.decision.is_empty() {
                "open"
            } else {
                &p.decision
            };
            let person = |x: &Option<String>| {
                x.as_ref()
                    .map(|n| format!("[[People/{n}|{n}]]"))
                    .unwrap_or_else(|| "unknown".into())
            };
            let where_ = if p.day.is_empty() {
                String::new()
            } else {
                format!(
                    " in [[{proj}/Chat/{}/{}|{} / {}]]",
                    p.thread, p.day, p.thread, p.day
                )
            };
            files.push((
                format!("{proj}/Permissions/{id}.md"),
                format!(
                    "---\nproject: {project}\npermission: {id}\nstatus: {status}\ntags: [claudecord, permission]\n---\n# {id}\n\nProject: [[{proj}/index|{project}]]\n\n{} wanted to do ({}){where_}:\n\n{}\nDecision: {status}{}\n",
                    person(&p.agent),
                    p.kind,
                    p.action.lines().map(|l| format!("> {l}\n")).collect::<String>(),
                    p.by.as_ref().map(|(n, _)| format!(" (by [[People/{n}|{n}]])")).unwrap_or_default(),
                ),
            ));
            perm_list.push(format!("- [[{proj}/Permissions/{id}|{id}]] ({status})\n"));
        }
        let members: Vec<String> = people_in
            .iter()
            .filter(|(_, v)| v.1.contains(&proj))
            .map(|(n, _)| format!("- [[People/{n}|{n}]]\n"))
            .collect();
        files.push((
            format!("{proj}/index.md"),
            format!("---\nproject: {project}\ntags: [claudecord, project]\n---\n# {project}\n\n## Threads\n{}\n## People\n{}\n## Tasks\n{}\n## Questions\n{}\n## Permissions\n{}", thread_list.concat(), members.concat(), task_list.concat(), ask_list.concat(), perm_list.concat()),
        ));
    }
    for (name, (kind, projects)) in &people_in {
        let links: String = projects
            .iter()
            .map(|p| format!("- [[{p}/index|{p}]]\n"))
            .collect();
        files.push((format!("People/{name}.md"), format!("---\nname: {name}\nkind: {kind}\ntags: [claudecord, {kind}]\n---\n# {name}\n\nSeen in:\n{links}\nEvery message {name} sent or was named in appears under Backlinks.\n")));
    }
    let projects: String = by_project
        .keys()
        .map(|p| format!("- [[{0}/index|{p}]]\n", file_name(p)))
        .collect();
    files.push(("Home.md".into(), format!("---\ntags: [claudecord]\n---\n# claudeCord\n\n{projects}\nOpen the graph view to see how people, threads and tasks connect.\n")));
    files
}

/// What is known about a question from the history.
#[derive(Default)]
struct AskNote {
    asker: Option<String>,
    question: String,
    /// The answerer's note name and how they were shown ("sam (operator)").
    answerer: Option<(String, String)>,
    answer: String,
    thread: String,
    day: String,
}

/// What is known about a permission request from the history.
#[derive(Default)]
struct PermNote {
    agent: Option<String>,
    kind: String,
    action: String,
    decision: String,
    by: Option<(String, String)>,
    thread: String,
    day: String,
}

/// What is known about a task from the history.
#[derive(Default)]
struct TaskNote {
    status: String,
    assignee: Option<String>,
    lead: Option<String>,
    text: String,
}

/// Colours for Obsidian's graph, written only if the vault has no graph settings yet: people, tasks and threads each get
/// their own colour so the clusters are easy to read.
const GRAPH_SETTINGS: &str = r#"{
  "colorGroups": [
    { "query": "tag:#human", "color": { "a": 1, "rgb": 5431378 } },
    { "query": "tag:#agent", "color": { "a": 1, "rgb": 14701138 } },
    { "query": "tag:#task", "color": { "a": 1, "rgb": 16098851 } },
    { "query": "tag:#thread", "color": { "a": 1, "rgb": 11720162 } }
  ],
  "showTags": false,
  "showOrphans": false
}
"#;

/// Writes the files into the vault folder, creating folders as needed and touching only files whose content changed.
/// Returns how many files were written.
pub fn write_vault(vault: &Path, files: &[NoteFile]) -> std::io::Result<usize> {
    let mut written = 0;
    for (rel, content) in files {
        let path = vault.join(rel);
        if std::fs::read_to_string(&path).is_ok_and(|old| old == *content) {
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, content)?;
        written += 1;
    }
    let graph = vault.join(".obsidian").join("graph.json");
    if !graph.exists() {
        std::fs::create_dir_all(vault.join(".obsidian"))?;
        std::fs::write(graph, GRAPH_SETTINGS)?;
    }
    Ok(written)
}

/// Mentions found in a text, exposed for tests.
pub fn mentions_in(text: &str) -> Vec<String> {
    let (_, l) = link_text("p", text);
    l.people
        .into_iter()
        .chain(l.tasks)
        .chain(l.asks)
        .chain(l.perms)
        .collect()
}

/// Task ids found in a text, exposed for tests.
pub fn tasks_in(text: &str) -> Vec<String> {
    task_ids(text)
}
