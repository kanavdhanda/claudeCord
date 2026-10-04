//! The tmux backend against a real tmux (each test skips itself where tmux is not installed): start, paste, type, read,
//! see the program end, and stop. Each test uses its own private tmux server so they never touch each other or the person's own tmux.
#![cfg(unix)]

use claudecord::device::inject::Guard;
use claudecord::device::tmux::{TmuxTerminal, stop_server};
use std::path::Path;
use std::time::Duration;

/// A running session of `argv` on a server of its own, or None where tmux is missing.
fn start(name: &str, argv: &[&str]) -> Option<(TmuxTerminal, String)> {
    if !TmuxTerminal::available() {
        return None;
    }
    let socket = format!("cct-{name}-{}", std::process::id());
    let t = TmuxTerminal::spawn(
        &socket,
        "s",
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        Path::new("/tmp"),
        &["HOME".to_string()],
        &[("ADDED".into(), "yes".into())],
        24,
        80,
        Guard::with_times(0, 100, 100),
    )
    .expect("tmux session starts");
    Some((t, socket))
}

/// Looks at the session until `want` shows on the screen.
async fn shows(t: &TmuxTerminal, want: &str) -> bool {
    for _ in 0..60 {
        t.observe(claudecord::now_ms() + 1_000_000);
        if t.screen_text().contains(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn a_paste_arrives_as_one_message_and_a_half_typed_line_is_not_pasted_over() {
    let Some((t, socket)) = start("paste", &["cat"]) else {
        return;
    };
    let now = claudecord::now_ms() + 10_000;
    assert!(t.inject("hello \u{1b}[31mworld", now).is_ok());
    assert!(shows(&t, "hello").await);
    assert!(
        !t.screen_text().contains('\u{1b}'),
        "control characters never reach the program"
    );
    t.type_input(b"half", now + 1000).unwrap();
    assert!(shows(&t, "half").await);
    // tmux cannot see a half-typed line, but the activity it just caused holds the next paste back.
    assert!(t.inject("x", now + 1500).is_err());
    stop_server(&socket);
}

#[tokio::test]
async fn the_program_gets_the_variables_it_was_given_and_none_it_was_denied() {
    let Some((t, socket)) = start(
        "env",
        &["sh", "-c", "echo home=[$HOME] added=[$ADDED]; sleep 30"],
    ) else {
        return;
    };
    assert!(
        shows(&t, "home=[] added=[yes]").await,
        "{}",
        t.screen_text()
    );
    stop_server(&socket);
}

#[tokio::test]
async fn an_ended_program_leaves_its_last_screen_and_is_seen_as_ended() {
    let Some((t, socket)) = start("dead", &["sh", "-c", "echo goodbye; sleep 1"]) else {
        return;
    };
    assert!(shows(&t, "goodbye").await, "the last screen stays readable");
    for _ in 0..30 {
        t.observe(claudecord::now_ms() + 2_000_000);
        if t.has_exited() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(t.has_exited());
    stop_server(&socket);
}

#[tokio::test]
async fn a_stopped_session_is_seen_as_gone_and_the_attach_command_names_it() {
    let Some((t, socket)) = start("kill", &["cat"]) else {
        return;
    };
    let cmd = t.attach_command();
    assert!(cmd.contains(&socket) && cmd.contains(&"attach-session".to_string()));
    t.kill();
    t.observe(claudecord::now_ms() + 3_000_000);
    assert!(t.has_exited());
    stop_server(&socket);
}
