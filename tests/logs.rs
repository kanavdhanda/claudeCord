//! The per-agent log files: what they keep, how they are trimmed and what is removed before anyone reads them.

use claudecord::device::logs::{AgentLog, strip_ansi};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-logs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn terminal_control_codes_are_stripped_and_text_is_kept() {
    assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text\r\n"), "red text\n");
    assert_eq!(strip_ansi("a\u{1b}]0;window title\u{7}b"), "ab");
    assert_eq!(strip_ansi("a\u{1b}]0;title\u{1b}\\b"), "ab");
    assert_eq!(strip_ansi("up\u{1b}[2Adown\u{1b}[Kend"), "updownend");
    assert_eq!(strip_ansi("keep\ttabs\nand lines"), "keep\ttabs\nand lines");
}

#[test]
fn events_are_written_with_secrets_removed_and_read_back_in_order() {
    let home = tmp("events");
    let log = AgentLog::new(&home, "demo/otter");
    log.event(1, "say", "first");
    log.event(2, "say", &format!("key ghp_{}", "b".repeat(36)));
    log.event(3, "done", "T1: finished");
    let tail = log.tail_events(10);
    assert_eq!(tail.len(), 3);
    assert!(tail[0].starts_with("1 say: first"));
    assert!(!tail.join("\n").contains("ghp_"));
    assert_eq!(
        log.tail_events(2).len(),
        2,
        "only the newest are returned when fewer are asked for"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("logs/demo_otter"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the log folder is private");
    }
}

#[test]
fn the_terminal_log_is_read_back_as_plain_text_with_secrets_removed() {
    let home = tmp("term");
    let log = AgentLog::new(&home, "demo/otter");
    log.terminal(b"\x1b[1mhello\x1b[0m\r\n");
    log.terminal(format!("token ghp_{}\r\n", "c".repeat(36)).as_bytes());
    log.terminal(b"\r\n\r\nlast line\r\n");
    let tail = log.tail_terminal(10);
    assert_eq!(tail[0], "hello");
    assert!(
        tail[1].contains("[redacted") && !tail[1].contains("ghp_"),
        "{tail:?}"
    );
    assert_eq!(tail.last().unwrap(), "last line", "blank lines are dropped");
}

#[test]
fn a_log_that_grows_too_large_keeps_its_newest_part() {
    let home = tmp("trim");
    let log = AgentLog::new(&home, "demo/otter");
    let line = "x".repeat(1000);
    for i in 0..6000 {
        log.terminal(format!("{i} {line}\n").as_bytes());
    }
    let size = std::fs::metadata(log.terminal_path()).unwrap().len();
    assert!(size <= 4 * 1024 * 1024, "trimmed to the limit, got {size}");
    assert!(
        log.tail_terminal(1)[0].starts_with("5999 "),
        "the newest line survived"
    );
}
