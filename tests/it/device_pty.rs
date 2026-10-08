//! The terminal side, with real pseudo-terminals and a stand-in agent (`cat`, which echoes what it is given). Covers the
//! paste rules, that a pasted message arrives as text and cannot escape the paste, and that the agent keeps running.
#![cfg(unix)]

use claudecord::device::inject::Guard as G;
use claudecord::device::inject::{Guard, Urgency, Wait, paste_bytes};
use claudecord::device::pty::PtyTerminal;
use std::time::Duration;

fn cat(now: i64) -> PtyTerminal {
    PtyTerminal::spawn(
        &["cat".into()],
        std::path::Path::new("/tmp"),
        &[],
        &[],
        24,
        80,
        G::new(now),
    )
    .expect("start cat")
}

async fn screen_has(a: &PtyTerminal, needle: &str) -> bool {
    for _ in 0..100 {
        if a.screen_text().contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[test]
fn the_guard_waits_for_the_person_and_for_the_agent() {
    let mut g = Guard::with_times(0, 2000, 3000);
    assert_eq!(
        g.check(1000, true),
        Err(Wait::AgentBusy),
        "just started, still printing"
    );
    assert_eq!(g.check(5000, true), Ok(()));
    g.on_input(b"hel", 5000);
    assert_eq!(
        g.check(9000, true),
        Err(Wait::PartialLine),
        "a half-typed line blocks for as long as it is there"
    );
    g.on_input(b"\r", 9000);
    assert_eq!(g.check(9500, true), Err(Wait::PersonTyping));
    assert_eq!(g.check(11_100, true), Ok(()));
    g.on_input(b"abc", 12_000);
    g.on_input(&[0x15], 12_100);
    assert_eq!(g.check(15_000, true), Ok(()), "Ctrl-U clears the line");
    g.on_output(15_100);
    assert_eq!(g.check(16_000, true), Err(Wait::AgentBusy));
    assert_eq!(g.check(19_000, false), Err(Wait::NotForeground));
}

#[test]
fn an_urgent_message_does_not_wait_for_the_agent_to_go_quiet() {
    let mut g = Guard::with_times(0, 2000, 3000);
    g.on_output(10_000);
    assert_eq!(
        g.check_as(10_500, true, Urgency::Queue),
        Err(Wait::AgentBusy)
    );
    assert_eq!(g.check_as(10_500, true, Urgency::Now), Ok(()));
    assert_eq!(g.check_as(10_500, true, Urgency::Steer), Ok(()));
    g.on_input(b"hel", 11_000);
    assert_eq!(
        g.check_as(11_500, true, Urgency::Now),
        Err(Wait::PartialLine),
        "going through does not trample a line a person is typing"
    );
    assert_eq!(
        g.check_as(11_500, true, Urgency::Steer),
        Ok(()),
        "steering is the person taking the wheel"
    );
    assert_eq!(
        g.check_as(11_500, false, Urgency::Steer),
        Err(Wait::NotForeground),
        "but never into a program that is not the agent"
    );
}

#[test]
fn a_pasted_message_cannot_end_the_paste_early_or_carry_escape_codes() {
    let evil = "hi\x1b[201~rm -rf /\x1b[2J\u{9b}31m";
    let b = paste_bytes(evil);
    let s = String::from_utf8_lossy(&b);
    assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~\r"));
    assert_eq!(
        s.matches("\x1b[201~").count(),
        1,
        "only the real end marker is present"
    );
    assert!(!s.contains("\x1b[2J") && !s.contains('\u{9b}'));
}

#[tokio::test]
async fn a_message_pasted_into_a_real_terminal_arrives_as_text() {
    let a = cat(0);
    // Long enough since start that the agent counts as quiet.
    let now = claudecord::now_ms() + 10_000;
    assert_eq!(a.inject("hello from the hub", now), Ok(()));
    assert!(screen_has(&a, "hello from the hub").await);
    a.kill();
}

#[tokio::test]
async fn nothing_is_pasted_over_a_half_typed_line() {
    let a = cat(0);
    let now = claudecord::now_ms() + 10_000;
    a.type_input(b"half", now).unwrap();
    assert_eq!(a.inject("must wait", now + 5000), Err(Wait::PartialLine));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!a.screen_text().contains("must wait"));
    a.type_input(b"\r", now + 5000).unwrap();
    assert_eq!(a.inject("now it goes", now + 20_000), Ok(()));
    assert!(screen_has(&a, "now it goes").await);
    a.kill();
}

#[tokio::test]
async fn the_agent_keeps_running_and_its_end_is_noticed() {
    let a = PtyTerminal::spawn(
        &["sh".into(), "-c".into(), "sleep 0.3; echo done".into()],
        std::path::Path::new("/tmp"),
        &[],
        &[],
        24,
        80,
        G::new(0),
    )
    .unwrap();
    assert!(!a.has_exited());
    assert!(screen_has(&a, "done").await);
    for _ in 0..50 {
        if a.has_exited() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the exit was not noticed");
}

#[tokio::test]
async fn secrets_are_removed_from_the_agents_environment() {
    // Safe enough here: set before the agent starts, and no other test reads this variable.
    unsafe { std::env::set_var("CC_TEST_SECRET_TOKEN", "hunter2") };
    let a = PtyTerminal::spawn(
        &[
            "sh".into(),
            "-c".into(),
            "echo SECRET=[$CC_TEST_SECRET_TOKEN] KEPT=[$HOME]".into(),
        ],
        std::path::Path::new("/tmp"),
        &["CC_TEST_SECRET_TOKEN".into()],
        &[],
        24,
        120,
        G::new(0),
    )
    .unwrap();
    assert!(
        screen_has(&a, "SECRET=[]").await,
        "screen: {}",
        a.screen_text()
    );
    assert!(!a.screen_text().contains("hunter2"));
}

#[tokio::test]
async fn pastes_from_many_threads_each_arrive_whole_and_never_mixed() {
    use claudecord::device::inject::Guard;
    use claudecord::device::pty::PtyTerminal;
    let t = std::sync::Arc::new(
        PtyTerminal::spawn(
            &["cat".into()],
            std::path::Path::new("/tmp"),
            &[],
            &[],
            40,
            120,
            Guard::with_times(0, 0, 0),
        )
        .unwrap(),
    );
    let now = claudecord::now_ms() + 100_000;
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let t = t.clone();
            std::thread::spawn(move || {
                let marker = format!("<{i}:{}>", "abcdefgh".repeat(4));
                (marker.clone(), t.inject(&marker, now).is_ok())
            })
        })
        .collect();
    let sent: Vec<_> = threads
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|(_, ok)| *ok)
        .collect();
    assert!(!sent.is_empty(), "at least some pastes were accepted");
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let screen = t.screen_text();
    // A paste is one write, so each marker appears in one piece. Terminal wrapping at the column width could split it across
    // lines, so the screen is compared with the line breaks removed.
    let flat: String = screen.split_whitespace().collect();
    for (marker, _) in &sent {
        assert!(
            flat.contains(marker.as_str()),
            "{marker} is missing or broken up in: {screen}"
        );
    }
    t.kill();
}
