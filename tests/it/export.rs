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
