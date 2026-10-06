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
                // Few enough commits that a slow disk (every commit is synced) never makes one writer wait out the lock timeout.
                for i in 0..25 {
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
            for line in (100..300).step_by(50) {
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
    assert_eq!(texts.len(), 4 * 25 * 5, "no row lost");
    texts.sort();
    texts.dedup();
    assert_eq!(texts.len(), 4 * 25 * 5, "no row twice");
}

#[test]
fn a_project_cannot_have_more_than_its_limit_of_agents_and_the_ones_it_has_are_untouched() {
    use claudecord::hub::{Chat, Effect, HubCore, MAX_AGENTS_PER_PROJECT};
    use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
    let mut c = HubCore::default();
    let register = |c: &mut HubCore, name: &str| {
        c.on_node_frame(
            "mac",
            NodeFrame::AgentRegister {
                agent: AgentSpec {
                    agent_id: format!("demo/{name}"),
                    name: name.into(),
                    project: "demo".into(),
                    adapter: AdapterId::Claude,
                    model: None,
                    role: None,
                },
                cwd: "/x".into(),
            },
            0,
        )
    };
    for i in 0..MAX_AGENTS_PER_PROJECT {
        register(&mut c, &format!("a{i}"));
    }
    assert_eq!(c.agents_of_project("demo").len(), MAX_AGENTS_PER_PROJECT);
    let fx = register(&mut c, "one-too-many");
    assert_eq!(
        c.agents_of_project("demo").len(),
        MAX_AGENTS_PER_PROJECT,
        "the extra agent was refused"
    );
    assert!(c.agent("demo/one-too-many").is_none());
    assert!(
        fx.iter().any(
            |e| matches!(e, Effect::Chat(Chat::Notice { text, .. }) if text.contains("limit"))
        ),
        "the chat is told why"
    );
    // An agent that is already there registering again (a restart) is not counted twice and not refused.
    register(&mut c, "a0");
    assert_eq!(c.agents_of_project("demo").len(), MAX_AGENTS_PER_PROJECT);
}

#[test]
fn a_damaged_database_is_found_by_the_integrity_check_and_a_sound_one_passes() {
    let dir = std::env::temp_dir().join(format!("cc-edge-integrity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hub.db");
    let mut store = Store::open(&path, None).unwrap();
    let rows: Vec<_> = (0..2000)
        .map(|i| HistoryRow {
            id: 0,
            at: i,
            project: "p".into(),
            thread: None,
            from: "w".into(),
            kind: "say".into(),
            text: format!("row {i} {}", "x".repeat(200)),
        })
        .collect();
    store.append(&rows).unwrap();
    assert!(store.integrity().is_ok());
    drop(store);
    // Overwrite a page in the middle of the file with junk.
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 20_000);
    for b in &mut bytes[8_192..12_288] {
        *b = 0xA5;
    }
    std::fs::write(&path, &bytes).unwrap();
    // Damage is noticed either by refusing to open the file or by the check, and either way a person is told.
    let noticed = match Store::open(&path, None) {
        Err(_) => true,
        Ok(s) => s.integrity().is_err(),
    };
    assert!(noticed, "damage went unnoticed");
}

#[tokio::test]
async fn a_step_that_panics_is_reported_as_none_and_the_next_step_still_runs() {
    use claudecord::task::guarded;
    let first: Option<u32> = guarded("a step that fails", async {
        if true {
            panic!("on purpose")
        }
        1
    })
    .await;
    assert_eq!(first, None);
    let second = guarded("the next step", async { 2 }).await;
    assert_eq!(second, Some(2));
}

#[test]
fn agent_names_follow_discords_rules_for_names_that_are_posted_and_mentioned() {
    use claudecord::protocol::agent_name_problem as bad;
    for ok in ["otter", "macbook-eeg-main", "keen-otter", "a_b.c", "Otter"] {
        assert_eq!(bad(ok), None, "{ok}");
    }
    for no in [
        "",
        "has space",
        "discord-bot",
        "My-Clyde",
        "everyone",
        "HERE",
        &"x".repeat(33),
    ] {
        assert!(bad(no).is_some(), "{no:?} should be refused");
    }
}

/// The start-up question exactly as the real Claude Code showed it in a new folder (captured from a running agent): options WITHOUT numbers, the
/// cursor on "No, exit", blank lines between the paragraphs and before the footer.
const TRUST_SCREEN: &str = "\n────────────────────────────────────────────────────────────────────────────────────────────────────\n Accessing workspace:\n\n /home/me/code/eeg\n\n Quick safety check: Is this a project you created or one you trust? (Like your own code, a\n well-known open source project, or work from your team). If not, take a moment to review what's in\n this folder first.\n\n Claude Code'll be able to read, edit, and execute files here.\n\n Security guide\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel\n";

#[test]
fn a_start_up_question_without_numbers_is_seen_so_it_reaches_the_chat_instead_of_hanging_the_agent()
{
    use claudecord::adapters::parse_menu;
    use claudecord::protocol::AdapterId;
    let p = parse_menu(TRUST_SCREEN).expect("the unnumbered menu is recognised");
    assert_eq!(p.options, vec!["No, exit", "Yes, I trust this folder"]);
    assert_eq!(p.cursor, 0, "the cursor starts on the safe choice");
    assert!(p.question.contains("trust"), "{}", p.question);
    // The agent is then not 'ready' (it used to look idle because of the cursor mark), and the right answer is the second option.
    let st = AdapterId::Claude.detect(TRUST_SCREEN);
    assert!(st.prompt.is_some() && !st.ready, "{st:?}");
    assert_eq!(AdapterId::Claude.startup_choice(&p), Some(1));
    // Down once, Enter: what is typed when someone allows it.
    assert_eq!(
        AdapterId::Claude.select_keys(&p, 1),
        vec!["Down".to_string(), "Enter".to_string()]
    );
}

#[test]
fn an_idle_screen_and_ordinary_text_are_not_mistaken_for_a_menu() {
    use claudecord::adapters::parse_menu;
    use claudecord::protocol::AdapterId;
    let idle = "\n╭────────────────────────────────╮\n│ > Try \"fix the failing test\"   │\n╰────────────────────────────────╯\n  ? for shortcuts\n";
    assert!(parse_menu(idle).is_none());
    assert!(AdapterId::Claude.detect(idle).ready);
    // Two indented lines above a footer but no cursor mark: not a menu.
    assert!(parse_menu("  one\n  two\n Enter to confirm\n").is_none());
}

#[test]
fn text_for_discord_gets_real_line_breaks_and_tables_in_a_code_block() {
    use claudecord::discord::api::tidy_for_discord as tidy;
    // A shell leaves a literal backslash-n; with no real line break in the text it is read as one.
    assert_eq!(tidy("one\\ntwo"), "one\ntwo");
    // With real line breaks, a backslash-n is the agent's own (code, a path) and stays.
    assert_eq!(tidy("a\nprint('x\\n')"), "a\nprint('x\\n')");
    // Two or more table rows in a row go in a code block; one line with bars is just text.
    let table = "Results:\n| a | b |\n|---|---|\n| 1 | 2 |\ndone";
    assert_eq!(
        tidy(table),
        "Results:\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n```\ndone"
    );
    assert_eq!(tidy("a | b |"), "a | b |");
    assert_eq!(tidy("plain text"), "plain text");
}

#[test]
fn the_file_checksum_is_the_standard_sha256() {
    // Published test vectors (FIPS 180-2), the same as the `shasum -a 256` tool gives.
    use claudecord::agents::text::sha256_hex;
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
