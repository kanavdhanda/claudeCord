//! The Obsidian export: what notes it makes, that the links are right so the graph shows real connections, that running
//! it again changes nothing, and that it includes history that has moved into compressed files.

use claudecord::export::obsidian::{mentions_in, render, tasks_in, write_vault};
use claudecord::store::{HistoryRow, Store};
use std::collections::HashMap;

fn row(id: i64, at: i64, thread: Option<&str>, from: &str, kind: &str, text: &str) -> HistoryRow {
    HistoryRow {
        id,
        at,
        project: "demo".into(),
        thread: thread.map(String::from),
        from: from.into(),
        kind: kind.into(),
        text: text.into(),
    }
}

// 2013-05-24 12:30:00 UTC
const T: i64 = 1_369_398_600_000;

fn sample() -> Vec<HistoryRow> {
    vec![
        row(
            1,
            T,
            None,
            "kd (owner)",
            "human",
            "Add retries to the client",
        ),
        row(
            2,
            T + 60_000,
            Some("T1"),
            "otter",
            "task",
            "T1 to heron: implement retry",
        ),
        row(
            3,
            T + 120_000,
            Some("T1"),
            "otter",
            "say",
            "@heron use jitter\nand keep the API",
        ),
        row(
            4,
            T + 180_000,
            Some("T1"),
            "heron",
            "say",
            "On it, T1 started",
        ),
        row(5, T + 240_000, Some("T1"), "system", "task", "T1 Done"),
    ]
}

fn files() -> HashMap<String, String> {
    render(&sample()).into_iter().collect()
}

#[test]
fn every_kind_of_note_is_made_in_the_expected_place() {
    let f = files();
    for path in [
        "Home.md",
        "demo/index.md",
        "demo/Chat/general/index.md",
        "demo/Chat/general/2013-05-24.md",
        "demo/Chat/T1/index.md",
        "demo/Chat/T1/2013-05-24.md",
        "demo/Tasks/T1.md",
        "People/kd.md",
        "People/otter.md",
        "People/heron.md",
    ] {
        assert!(
            f.contains_key(path),
            "missing {path}; have {:?}",
            f.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        !f.contains_key("People/system.md"),
        "the system is not a person"
    );
}

#[test]
fn messages_link_to_the_people_and_tasks_they_name_and_up_to_their_thread() {
    let day = &files()["demo/Chat/T1/2013-05-24.md"];
    assert!(day.contains("Up: [[demo/Chat/T1/index|T1]]"), "{day}");
    assert!(day.contains("12:32 **[[People/otter|otter]]** (say): [[People/heron|@heron]] use jitter\n  and keep the API"), "{day}");
    assert!(day.contains("On it, [[demo/Tasks/T1|T1]] started"), "{day}");
    assert!(day.contains("project: demo") && day.contains("date: 2013-05-24"));
    let general = &files()["demo/Chat/general/2013-05-24.md"];
    assert!(
        general.contains("12:30 **[[People/kd|kd (owner)]]** (human): Add retries"),
        "{general}"
    );
}

#[test]
fn people_tasks_and_threads_are_tied_together_so_the_graph_has_structure() {
    let f = files();
    let task = &f["demo/Tasks/T1.md"];
    assert!(
        task.contains("status: done")
            && task.contains("Given by: [[People/otter|otter]]")
            && task.contains("Assigned to: [[People/heron|heron]]"),
        "{task}"
    );
    assert!(task.contains("implement retry"));
    let kd = &f["People/kd.md"];
    assert!(
        kd.contains("kind: human") && kd.contains("[[demo/index|demo]]"),
        "{kd}"
    );
    assert!(f["People/heron.md"].contains("kind: agent"));
    let idx = &f["demo/index.md"];
    assert!(
        idx.contains("[[demo/Chat/T1/index|T1]]")
            && idx.contains("[[People/otter|otter]]")
            && idx.contains("[[demo/Tasks/T1|T1]] (done)"),
        "{idx}"
    );
    assert!(
        f["demo/Chat/T1/index.md"].contains("[[demo/Chat/T1/2013-05-24|2013-05-24]] (4 messages)")
    );
    assert!(f["Home.md"].contains("[[demo/index|demo]]"));
}

#[test]
fn mentions_and_task_ids_are_found_only_where_they_really_are() {
    assert_eq!(mentions_in("hi @heron, see T12."), vec!["heron", "T12"]);
    assert_eq!(
        tasks_in("T1 and T22, but not AT3 or T4x or T"),
        vec!["T1", "T22"]
    );
    assert!(
        mentions_in("mail me@example.com").is_empty(),
        "an address is not a mention"
    );
}

#[test]
fn writing_again_changes_nothing_and_a_new_message_rewrites_only_what_it_touches() {
    let dir = std::env::temp_dir().join(format!("cc-obs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let n = write_vault(&dir, &render(&sample())).unwrap();
    assert!(n >= 10);
    assert_eq!(
        write_vault(&dir, &render(&sample())).unwrap(),
        0,
        "nothing changed, nothing written"
    );
    assert!(
        dir.join(".obsidian/graph.json").exists(),
        "graph colours are set up"
    );
    std::fs::write(dir.join(".obsidian/graph.json"), "{\"mine\":true}").unwrap();
    let mut more = sample();
    more.push(row(6, T + 300_000, Some("T1"), "heron", "say", "all done"));
    let n = write_vault(&dir, &render(&more)).unwrap();
    assert!(
        (1..=5).contains(&n),
        "only the touched notes were rewritten, got {n}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".obsidian/graph.json")).unwrap(),
        "{\"mine\":true}",
        "the person's own graph settings are never overwritten"
    );
    assert!(
        std::fs::read_to_string(dir.join("demo/Chat/T1/2013-05-24.md"))
            .unwrap()
            .contains("all done")
    );
}

#[test]
fn hostile_names_cannot_write_outside_the_vault() {
    let rows = vec![row(
        1,
        T,
        Some("../../etc"),
        "../evil (owner)",
        "human",
        "x",
    )];
    for (path, _) in render(&rows) {
        assert!(!path.contains(".."), "{path}");
        assert!(!path.starts_with('/'), "{path}");
    }
}

#[test]
fn history_that_moved_into_compressed_files_is_still_exported() {
    let dir = std::env::temp_dir().join(format!("cc-obs-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).unwrap();
    let mut rows = sample();
    rows.push(row(
        0,
        T + 10_000_000_000,
        None,
        "kd (owner)",
        "human",
        "recent",
    ));
    for r in &mut rows {
        r.id = 0;
    }
    s.append(&rows).unwrap();
    assert_eq!(
        s.rollover(T + 1_000_000).unwrap(),
        5,
        "the five old rows left the database"
    );
    assert_eq!(s.projects().unwrap(), vec!["demo"]);
    let all = s.history_all("demo").unwrap();
    assert_eq!(
        all.len(),
        6,
        "old rows from the file and the recent row from the database"
    );
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    let f: HashMap<String, String> = render(&all).into_iter().collect();
    assert!(
        f["demo/Chat/T1/2013-05-24.md"].contains("use jitter"),
        "old history shows up in the vault"
    );
}

fn qa_rows() -> Vec<HistoryRow> {
    vec![
        row(1, T, Some("T1"), "otter", "ask", "Q1: which db?"),
        row(
            2,
            T + 60_000,
            Some("T1"),
            "sam (operator)",
            "answer",
            "Q1: postgres",
        ),
        row(
            3,
            T + 120_000,
            Some("T1"),
            "heron",
            "ask",
            "Q2: which port?",
        ),
        row(
            4,
            T + 180_000,
            Some("T1"),
            "otter",
            "permission",
            "P1 bash: cargo test",
        ),
        row(
            5,
            T + 240_000,
            Some("T1"),
            "kd (owner)",
            "decision",
            "P1 allowed once by kd (owner)",
        ),
        row(
            6,
            T + 300_000,
            Some("T1"),
            "otter",
            "say",
            "going with Q1 and P1",
        ),
    ]
}

#[test]
fn a_question_and_its_answer_become_one_note_that_links_both_ways() {
    let f: HashMap<String, String> = render(&qa_rows()).into_iter().collect();
    let q1 = &f["demo/Asks/Q1.md"];
    assert!(
        q1.contains("status: answered")
            && q1.contains("asked_by: otter")
            && q1.contains("answered_by: sam"),
        "{q1}"
    );
    assert!(
        q1.contains(
            "Asked by [[People/otter|otter]] in [[demo/Chat/T1/2013-05-24|T1 / 2013-05-24]]"
        ),
        "{q1}"
    );
    assert!(q1.contains("> which db?"), "{q1}");
    assert!(
        q1.contains("Answered by [[People/sam|sam (operator)]]:\n\n> postgres"),
        "{q1}"
    );
    let day = &f["demo/Chat/T1/2013-05-24.md"];
    assert!(
        day.contains("(ask): [[demo/Asks/Q1|Q1]]: which db?"),
        "the question line links to its note: {day}"
    );
    assert!(
        day.contains("(answer): [[demo/Asks/Q1|Q1]]: postgres"),
        "the answer line links to the same note: {day}"
    );
    assert!(
        day.contains("going with [[demo/Asks/Q1|Q1]] and [[demo/Permissions/P1|P1]]"),
        "mentions anywhere link too: {day}"
    );
    assert!(f["People/sam.md"].contains("kind: agent") || f["People/sam.md"].contains("name: sam"));
}

#[test]
fn an_unanswered_question_says_so() {
    let f: HashMap<String, String> = render(&qa_rows()).into_iter().collect();
    let q2 = &f["demo/Asks/Q2.md"];
    assert!(
        q2.contains("status: open")
            && q2.contains("Not answered yet.")
            && q2.contains("> which port?"),
        "{q2}"
    );
}

#[test]
fn a_permission_request_and_its_decision_become_one_note() {
    let f: HashMap<String, String> = render(&qa_rows()).into_iter().collect();
    let p1 = &f["demo/Permissions/P1.md"];
    assert!(
        p1.contains("status: allowed once by kd (owner)")
            && p1.contains("> cargo test")
            && p1.contains("(by [[People/kd|kd]])"),
        "{p1}"
    );
    assert!(
        p1.contains("[[People/otter|otter]] wanted to do (bash)"),
        "{p1}"
    );
    let idx = &f["demo/index.md"];
    assert!(
        idx.contains("## Questions")
            && idx.contains("[[demo/Asks/Q1|Q1]] (answered)")
            && idx.contains("[[demo/Asks/Q2|Q2]] (open)"),
        "{idx}"
    );
    assert!(
        idx.contains("## Permissions") && idx.contains("[[demo/Permissions/P1|P1]]"),
        "{idx}"
    );
}

fn read_vault(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.insert(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    std::fs::read_to_string(&p).unwrap(),
                );
            }
        }
    }
    let mut out = Default::default();
    walk(dir, dir, &mut out);
    out
}

/// A whole conversation on a real hub (a request, a plan, a task, a question and its answer, a permission, a report), part of it already
/// moved out of the database into compressed files, made into a vault by the real export command: everything is readable in Obsidian.
#[tokio::test]
async fn a_real_conversation_including_the_agents_report_becomes_a_readable_vault() {
    use claudecord::hub::{Answerer, Decision, HubCore, Human, MessageOpts};
    use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
    use claudecord::server::{self, Config};
    let root = std::env::temp_dir().join(format!("cc-export-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = root.join("hub");
    std::fs::create_dir_all(&data).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let store = Store::open(&data.join("hub.db"), Some(&data.join("history"))).unwrap();
    let hub = server::start(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        },
        core,
        store,
    )
    .await
    .unwrap();
    let kd = Human {
        id: "1".into(),
        name: "kd".into(),
    };
    let spec = |n: &str| AgentSpec {
        agent_id: format!("demo/{n}"),
        name: n.into(),
        project: "demo".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    };
    let kd2 = kd.clone();
    hub.call(move |c, now| {
        let mut fx = vec![];
        for n in ["otter", "heron"] {
            fx.extend(c.on_node_frame(
                "mac",
                NodeFrame::AgentRegister {
                    agent: spec(n),
                    cwd: "/x".into(),
                },
                now,
            ));
        }
        fx.extend(
            c.human_message(
                &kd2,
                "demo",
                "Build the export feature",
                &MessageOpts::default(),
                now,
            )
            .unwrap()
            .1,
        );
        let frames = [
            NodeFrame::AgentAssign {
                agent_id: "demo/otter".into(),
                to: "heron".into(),
                task: "write the csv endpoint".into(),
                thread: None,
            },
            NodeFrame::AgentSay {
                agent_id: "demo/otter".into(),
                text: "plan: heron takes the api, I take the ui".into(),
                thread: None,
            },
            NodeFrame::AgentAsk {
                agent_id: "demo/heron".into(),
                ask_id: "a1".into(),
                question: "csv or json for the export?".into(),
                options: None,
                thread: None,
            },
            NodeFrame::AgentPermission {
                agent_id: "demo/heron".into(),
                perm_id: "p1".into(),
                kind: "bash".into(),
                action: "cargo test".into(),
                thread: None,
            },
        ];
        for f in frames {
            fx.extend(c.on_node_frame("mac", f, now));
        }
        fx.extend(
            c.answer_ask(
                &Answerer::Human(kd2.clone()),
                "demo",
                "Q1",
                "csv, streamed",
                now,
            )
            .map(|o| o.effects)
            .unwrap_or_default(),
        );
        fx.extend(
            c.decide_permission(&kd2, "demo", "P1", Decision::Once, None, now)
                .unwrap_or_default(),
        );
        fx.extend(c.on_node_frame(
            "mac",
            NodeFrame::AgentTaskDone {
                agent_id: "demo/heron".into(),
                task_id: "T1".into(),
                summary: "endpoint streams csv".into(),
            },
            now,
        ));
        fx.extend(c.on_node_frame(
            "mac",
            NodeFrame::AgentReport {
                agent_id: "demo/otter".into(),
                title: "Export feature finished".into(),
                summary: "The csv export streams and the ui button works".into(),
                artifacts: Some(vec!["src/export.rs".into()]),
            },
            now,
        ));
        ((), fx)
    })
    .await;
    hub.shutdown().await;
    // The older part of the conversation has already moved out of the database into a compressed file.
    {
        let mut s = Store::open(&data.join("hub.db"), Some(&data.join("history"))).unwrap();
        s.rollover(claudecord::now_ms() + 1000).unwrap();
        assert_eq!(s.hot_rows().unwrap(), 0, "everything is in files now");
    }
    let vault = root.join("vault");
    claudecord::cli::export::run(claudecord::cli::export::ExportArgs {
        data: data.clone(),
        out: vault.clone(),
        project: None,
        watch: None,
    })
    .unwrap();
    let notes = read_vault(&vault);
    // `CLAUDECORD_SHOW_VAULT=1 cargo test a_real_conversation -- --nocapture` prints the notes, to see what a person would open.
    if std::env::var_os("CLAUDECORD_SHOW_VAULT").is_some() {
        for (k, v) in &notes {
            println!("==== {k}\n{v}");
        }
    }
    let all: String = notes.values().cloned().collect::<Vec<_>>().join("\n");
    for want in [
        "Build the export feature",
        "plan: heron takes the api",
        "Export feature finished",
        "The csv export streams and the ui button works",
        "csv or json for the export?",
        "csv, streamed",
        "cargo test",
        "write the csv endpoint",
    ] {
        assert!(
            all.contains(want),
            "the vault has no mention of {want:?}; notes: {:?}",
            notes.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        notes.keys().any(|k| k.starts_with("People/")),
        "a note for each person and agent"
    );
    assert!(
        notes
            .keys()
            .any(|k| k.contains("Q") || k.to_lowercase().contains("question")),
        "a note for the question and its answer: {:?}",
        notes.keys().collect::<Vec<_>>()
    );
    // Running it again changes nothing.
    claudecord::cli::export::run(claudecord::cli::export::ExportArgs {
        data,
        out: vault.clone(),
        project: None,
        watch: None,
    })
    .unwrap();
    assert_eq!(read_vault(&vault), notes);
}
