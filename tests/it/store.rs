//! Storage tests: history, saving and restoring the core, crash safety, and rolling old history into compressed files.

use claudecord::hub::*;
use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
use claudecord::store::{HistoryRow, Store};

fn row(at: i64, project: &str, thread: Option<&str>, text: &str) -> HistoryRow {
    HistoryRow {
        id: 0,
        at,
        project: project.into(),
        thread: thread.map(String::from),
        from: "kd (owner)".into(),
        kind: "human".into(),
        text: text.into(),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn history_is_kept_in_order_and_can_be_read_by_thread_and_page() {
    let mut s = Store::open_memory().unwrap();
    s.append(&[
        row(1, "p", None, "a"),
        row(2, "p", Some("T1"), "b"),
        row(3, "q", None, "other"),
        row(4, "p", Some("T1"), "c"),
    ])
    .unwrap();
    let all = s.history("p", None, 0, 10).unwrap();
    assert_eq!(
        all.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(s.history("p", Some("T1"), 0, 10).unwrap().len(), 2);
    let page = s.history("p", None, all[0].id, 1).unwrap();
    assert_eq!(page[0].text, "b", "paging continues after the last id seen");
}

#[test]
fn a_batch_that_fails_part_way_saves_nothing_at_all() {
    let mut s = Store::open_memory().unwrap();
    s.append(&[row(1, "p", None, "kept")]).unwrap();
    let huge = "x".repeat(100_001);
    let batch = [
        row(2, "p", None, "would be lost"),
        row(3, "p", None, &huge),
        row(4, "p", None, "never reached"),
    ];
    assert!(
        s.append(&batch).is_err(),
        "a message over the size limit is refused"
    );
    let left = s.history("p", None, 0, 10).unwrap();
    assert_eq!(
        left.len(),
        1,
        "the good rows before the bad one were rolled back too"
    );
    assert_eq!(left[0].text, "kept");
}

#[test]
fn data_survives_closing_and_reopening_the_file() {
    let dir = tmp("reopen");
    let path = dir.join("t.db");
    {
        let mut s = Store::open(&path, None).unwrap();
        s.append(&[row(1, "p", None, "hello")]).unwrap();
        s.save_snapshot("{\"x\":1}", 5).unwrap();
    }
    let s = Store::open(&path, None).unwrap();
    assert_eq!(s.history("p", None, 0, 10).unwrap()[0].text, "hello");
    assert_eq!(s.load_snapshot().unwrap().as_deref(), Some("{\"x\":1}"));
    let mut s = s;
    s.append(&[row(2, "p", None, "later")]).unwrap();
    let ids: Vec<i64> = s
        .history("p", None, 0, 10)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![1, 2], "ids keep rising after a restart");
}

#[test]
fn old_history_rolls_into_a_compressed_file_and_can_still_be_read() {
    let dir = tmp("roll");
    let mut s = Store::open(&dir.join("t.db"), Some(&dir.join("seg"))).unwrap();
    let long = "the quick brown fox jumps over the lazy dog ".repeat(5);
    let rows: Vec<HistoryRow> = (0..200)
        .map(|i| row(i, "p", None, &format!("{long}{i}")))
        .chain((1000..1010).map(|i| row(i, "p", None, "recent")))
        .collect();
    s.append(&rows).unwrap();
    let moved = s.rollover(500).unwrap();
    assert_eq!(moved, 200);
    assert_eq!(
        s.hot_rows().unwrap(),
        10,
        "only recent rows stay in the database"
    );
    let segs = s.segments("p").unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].rows, 200);
    let size = std::fs::metadata(dir.join("seg").join(&segs[0].file))
        .unwrap()
        .len();
    let raw: usize = rows[..200].iter().map(|r| r.text.len()).sum();
    assert!(
        (size as usize) * 5 < raw,
        "gzip shrinks repetitive chat at least 5x ({size} vs {raw})"
    );
    let back = s.read_segment(&segs[0].file).unwrap();
    assert_eq!(back.len(), 200);
    assert_eq!(back[7].text, format!("{long}7"));
    assert_eq!(s.rollover(500).unwrap(), 0, "nothing left to move");
}

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        agent_id: format!("p/{name}"),
        name: name.into(),
        project: "p".into(),
        adapter: AdapterId::Claude,
        model: None,
        role: None,
    }
}

#[test]
fn a_restarted_hub_keeps_roles_asks_grants_handoffs_and_waiting_messages() {
    let kd = Human {
        id: "1".into(),
        name: "kd".into(),
    };
    let mut core = HubCore::default();
    core.add_owner("1");
    core.node_connected("mac", 1);
    core.on_node_frame(
        "mac",
        NodeFrame::AgentRegister {
            agent: spec("otter"),
            cwd: "/x".into(),
        },
        10,
    );
    core.hold(&kd, true, "p", Some("otter"), 11).unwrap();
    core.human_message(&kd, "p", "waiting for you", &MessageOpts::default(), 12)
        .unwrap();
    core.on_node_frame(
        "mac",
        NodeFrame::AgentAsk {
            agent_id: "p/otter".into(),
            ask_id: "a1".into(),
            question: "which db?".into(),
            options: None,
            thread: None,
        },
        13,
    );
    core.grant(&kd, "p", Some("otter"), Some("edit"), None, 14)
        .unwrap();
    core.on_node_frame(
        "mac",
        NodeFrame::AgentHandoff {
            agent_id: "p/otter".into(),
            text: "goal: x".into(),
        },
        15,
    );

    let saved = core.snapshot();
    let dir = tmp("restart");
    let mut store = Store::open(&dir.join("t.db"), None).unwrap();
    store.save_snapshot(&saved, 16).unwrap();
    drop(store);

    // A new process: open the file, restore, and let the device reconnect.
    let store = Store::open(&dir.join("t.db"), None).unwrap();
    let mut core2 = HubCore::default();
    assert!(core2.restore(&store.load_snapshot().unwrap().unwrap()));
    assert_eq!(
        core2.role_of("p", "1"),
        Some(Role::Owner),
        "roles come back too"
    );
    assert_eq!(core2.asks_of("p").len(), 1);
    assert_eq!(core2.active_grants(20).len(), 1);
    assert_eq!(core2.handoff_of("p/otter").unwrap().text, "goal: x");
    assert_eq!(
        core2.status_of("p/otter"),
        claudecord::protocol::AgentStatus::Offline
    );
    core2.node_connected("mac", 7);
    let fx = core2.on_node_frame(
        "mac",
        NodeFrame::AgentRegister {
            agent: spec("otter"),
            cwd: "/x".into(),
        },
        30,
    );
    let texts: Vec<String> = fx
        .iter()
        .filter_map(|e| {
            if let Effect::Send {
                frame: claudecord::protocol::HubFrame::Deliver { text, .. },
                ..
            } = e
            {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "waiting for you"),
        "the message that was queued before the restart is delivered after it: {texts:?}"
    );
    // The ask was answered before the crash? No, it is still open and still answerable.
    assert!(
        core2
            .answer_ask(&Answerer::Human(kd.clone()), "p", "Q1", "postgres", 31)
            .is_ok()
    );
}

#[test]
fn an_unreadable_snapshot_is_refused_and_changes_nothing() {
    let mut core = HubCore::default();
    core.add_owner("1");
    assert!(!core.restore("not json"));
    assert!(!core.restore("{\"version\":999}"));
    assert_eq!(core.role_of("p", "1"), Some(Role::Owner));
}
