//! Edge cases and randomised checks: hostile or odd input must never panic, and rules that must hold for every input are
//! checked on many generated ones (from a fixed seed, so a failure repeats). Everything here runs on every platform.

use claudecord::agents::text::strip_control;
use claudecord::protocol::{HubFrame, NodeFrame};
use claudecord::security::redact::redact;
use claudecord::store::{HistoryRow, Store};
use claudecord::uptime::{Change, State, budget_left, outages, report};

/// A tiny repeatable random number generator (no dependency, same numbers every run).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const VALID: &[&str] = &[
    r#"{"t":"hello","nodeName":"mac","version":"1"}"#,
    r#"{"t":"agent.register","agent":{"agentId":"p/otter","name":"otter","project":"p","adapter":"claude"},"cwd":"/x"}"#,
    r#"{"t":"agent.say","agentId":"p/otter","text":"hi @heron","thread":"t1"}"#,
    r#"{"t":"agent.ask","agentId":"p/otter","askId":"a1","question":"csv?","options":["csv","json"]}"#,
    r#"{"t":"agent.status","agentId":"p/otter","status":"thinking"}"#,
];

#[test]
fn mutated_and_hostile_frames_never_panic_and_only_valid_ones_parse() {
    let mut rng = Rng(7);
    for _ in 0..20_000 {
        let mut bytes = VALID[rng.below(VALID.len() as u64) as usize]
            .as_bytes()
            .to_vec();
        for _ in 0..=rng.below(4) {
            match rng.below(4) {
                0 if !bytes.is_empty() => {
                    let i = rng.below(bytes.len() as u64) as usize;
                    bytes[i] = rng.next() as u8;
                }
                1 if !bytes.is_empty() => {
                    bytes.remove(rng.below(bytes.len() as u64) as usize);
                }
                2 => {
                    let i = rng.below(bytes.len() as u64 + 1) as usize;
                    bytes.insert(i, b"{}[]\":,\\\0\xff"[rng.below(10) as usize]);
                }
                _ => bytes.truncate(rng.below(bytes.len() as u64 + 1) as usize),
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = NodeFrame::parse(&text);
        let _ = HubFrame::parse(&text);
    }
    // Shapes a hostile device might try.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let long = format!(
        r#"{{"t":"agent.say","agentId":"p/otter","text":"{}"}}"#,
        "x".repeat(5_000_000)
    );
    for hostile in [
        "",
        "null",
        "[]",
        "{}",
        "{\"t\":null}",
        "{\"t\":\"hello\"}",
        &deep,
        &long,
        "\u{0}",
        "{\"t\":\"hello\",\"nodeName\":1}",
    ] {
        assert!(
            NodeFrame::parse(hostile).is_none(),
            "accepted {:.40}",
            hostile
        );
    }
    for v in VALID {
        assert!(NodeFrame::parse(v).is_some(), "refused a valid frame: {v}");
    }
}

#[test]
fn control_characters_never_survive_and_secrets_never_survive_wherever_they_sit() {
    let mut rng = Rng(11);
    let token = format!("ghp_{}", "a1B2".repeat(9));
    for _ in 0..5_000 {
        let mut s = String::new();
        for _ in 0..rng.below(60) {
            s.push(match rng.below(6) {
                0 => char::from_u32(rng.below(32) as u32).unwrap(),
                1 => '\u{1b}',
                2 => char::from_u32(0x80 + rng.below(0x700) as u32).unwrap_or('?'),
                3 => '\u{202e}',
                4 => ' ',
                _ => (b'a' + rng.below(26) as u8) as char,
            });
        }
        let clean = strip_control(&s);
        assert!(
            !clean
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{clean:?}"
        );
        let at = rng.below(s.chars().count() as u64 + 1) as usize;
        let planted: String = s
            .chars()
            .take(at)
            .chain(token.chars())
            .chain(s.chars().skip(at))
            .collect();
        assert!(
            !redact(&planted).text.contains(&token),
            "token survived in {planted:?}"
        );
    }
    // Very large input is handled, not choked on.
    let big = format!("{}{token}{}", "x".repeat(3_000_000), "y".repeat(3_000_000));
    assert!(!redact(&big).text.contains(&token));
}

#[test]
fn availability_arithmetic_holds_for_any_sequence_of_changes() {
    let mut rng = Rng(3);
    for _ in 0..2_000 {
        let mut at = 0i64;
        let log: Vec<Change> = (0..rng.below(12))
            .map(|_| {
                at += rng.below(5_000) as i64;
                Change {
                    at,
                    state: if rng.below(2) == 0 {
                        State::Up
                    } else {
                        State::Down
                    },
                }
            })
            .collect();
        let since = rng.below(30_000) as i64;
        let now = since + rng.below(30_000) as i64;
        let r = report(&log, since, now);
        assert_eq!(
            r.up_ms + r.down_ms + r.unknown_ms,
            now - since,
            "every moment is counted exactly once: {log:?} {since} {now}"
        );
        assert!(r.up_ms >= 0 && r.down_ms >= 0 && r.unknown_ms >= 0);
        let down_from_outages: i64 = outages(&log, since, now).iter().map(|(a, b)| b - a).sum();
        assert_eq!(
            down_from_outages, r.down_ms,
            "the listed outages add up to the downtime: {log:?} {since} {now}"
        );
        if let Some(a) = r.availability() {
            assert!((0.0..=1.0).contains(&a));
        }
        assert!(
            budget_left(&r, 1.0) <= 0 || r.down_ms == 0,
            "a 100% target allows no downtime"
        );
    }
    // A window of no length, and one that ends before it starts, are empty rather than wrong.
    assert_eq!(
        report(
            &[Change {
                at: 0,
                state: State::Up
            }],
            10,
            10
        )
        .availability(),
        None
    );
    let inverted = report(
        &[Change {
            at: 0,
            state: State::Up,
        }],
        10,
        5,
    );
    assert!(inverted.up_ms <= 0);
}

#[test]
fn history_written_while_old_rows_are_moved_out_is_neither_lost_nor_doubled() {
    let dir = std::env::temp_dir().join(format!("cc-edge-roll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (db, seg) = (dir.join("hub.db"), dir.join("seg"));
    drop(Store::open(&db, Some(&seg)).unwrap());
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (db, seg) = (db.clone(), seg.clone());
            std::thread::spawn(move || {
                let mut s = Store::open(&db, Some(&seg)).unwrap();
                for i in 0..100 {
                    let rows: Vec<_> = (0..5)
                        .map(|k| HistoryRow {
                            id: 0,
                            at: (i * 10 + k) as i64,
                            project: "p".into(),
                            thread: None,
                            from: "w".into(),
                            kind: "say".into(),
                            text: format!("w{w}-{i}-{k}"),
                        })
                        .collect();
                    s.append(&rows).unwrap();
                }
            })
        })
        .collect();
    // Meanwhile another connection keeps moving everything older than a moving line into compressed files.
    let mover = {
        let (db, seg) = (db.clone(), seg.clone());
        std::thread::spawn(move || {
            let mut s = Store::open(&db, Some(&seg)).unwrap();
            for line in (200..1000).step_by(100) {
                let _ = s.rollover(line);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    for w in writers {
        w.join().unwrap();
    }
    mover.join().unwrap();
    let s = Store::open(&db, Some(&seg)).unwrap();
    let mut texts: Vec<String> = s
        .history_all("p")
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect();
    assert_eq!(texts.len(), 4 * 100 * 5, "no row lost");
    texts.sort();
    texts.dedup();
    assert_eq!(texts.len(), 4 * 100 * 5, "no row twice");
}
