//! How to start each agent CLI and how to read its terminal. An adapter looks at a screenshot of the visible pane and
//! says whether the agent is busy, ready for input, showing a menu, or out of usage. Screen reading is heuristic and
//! the agy and Codex heuristics are generic and unverified against the live programs.

use crate::protocol::{AdapterId, LimitKind};
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

/// Compiles a pattern once at startup. The patterns are fixed in this file, so a bad one is a programming error.
fn re(p: &str) -> Regex {
    Regex::new(p).expect("adapter pattern")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Autonomous,
    Plan,
    Ask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptInfo {
    /// Stable text used to tell whether the same prompt is still showing.
    pub signature: String,
    pub question: String,
    pub options: Vec<String>,
    /// Index of the option the cursor is on.
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitInfo {
    pub kind: LimitKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenState {
    pub busy: bool,
    pub ready: bool,
    pub executing: bool,
    pub prompt: Option<PromptInfo>,
    pub limit: Option<LimitInfo>,
}

pub struct LaunchCtx<'a> {
    pub name: &'a str,
    pub model: Option<&'a str>,
    pub policy: Policy,
    /// An MCP config that exposes the claudecord tools, if the adapter uses one.
    pub mcp_config: Option<&'a str>,
}

/// The last `n` lines, with trailing blanks dropped.
pub fn tail(screen: &str, n: usize) -> String {
    let mut lines: Vec<&str> = screen.split('\n').map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let from = lines.len().saturating_sub(n);
    lines[from..].join("\n")
}

/// Parses a menu on the screen: numbered ("1. Yes", "> 2. No") or, as Claude Code's start-up questions are shown, plain lines with a cursor
/// marker ("❯ No, exit" above "  Yes, I trust this folder", ending in a line like "Enter to confirm").
pub fn parse_menu(screen: &str) -> Option<PromptInfo> {
    parse_numbered_menu(screen).or_else(|| parse_plain_menu(screen))
}

/// A menu whose options carry no numbers: the option lines end right above a footer such as "Enter to confirm · Esc to cancel", exactly one is
/// marked with the cursor, and the question is the text above them.
fn parse_plain_menu(screen: &str) -> Option<PromptInfo> {
    static FOOTER: LazyLock<Regex> =
        LazyLock::new(|| re(r"(?i)enter to (confirm|select)|esc to cancel"));
    static CURSOR: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*[❯>›]\s+(\S.*?)\s*$"));
    static DECOR: LazyLock<Regex> = LazyLock::new(|| re(r"[│╭╮╰╯─]"));
    let text = tail(screen, 30);
    let lines: Vec<&str> = text.split('\n').collect();
    let footer = lines.iter().rposition(|l| FOOTER.is_match(l))?;
    // The cursor line is the last one above the footer that carries the marker. Its text starts at some column; the other options are the
    // unbroken run of lines around it that are indented at least that far (the question above sits further left).
    let cur = lines[..footer].iter().rposition(|l| CURSOR.is_match(l))?;
    let col = {
        let l = lines[cur];
        let text_at = CURSOR.captures(l)?.get(1)?.start();
        l[..text_at].chars().count()
    };
    let indented = |l: &str| {
        !l.trim().is_empty()
            && !DECOR.is_match(l)
            && !CURSOR.is_match(l)
            && l.chars().take_while(|c| *c == ' ').count() >= col
    };
    let mut first = cur;
    while first > 0 && indented(lines[first - 1]) {
        first -= 1;
    }
    let mut end = cur + 1;
    while end < footer && indented(lines[end]) {
        end += 1;
    }
    // Between the last option and the footer there may be blank lines, and nothing else: anything else means this is not a menu.
    let mut gap = end;
    while gap < footer && lines[gap].trim().is_empty() {
        gap += 1;
    }
    if gap != footer {
        return None;
    }
    let options: Vec<String> = lines[first..end]
        .iter()
        .map(|l| match CURSOR.captures(l) {
            Some(c) => c[1].to_string(),
            None => l.trim().to_string(),
        })
        .collect();
    let cursor = cur - first;
    if options.len() < 2 {
        return None;
    }
    // The question: the text above the options, back to the line that rules off the dialog (or at most twelve lines), as one line.
    let above: Vec<String> = lines[..first]
        .iter()
        .rev()
        .take_while(|l| l.trim().is_empty() || !l.trim().chars().all(|c| "─━-═".contains(c)))
        .map(|l| DECOR.replace_all(l, "").trim().to_string())
        .filter(|l| !l.is_empty())
        .take(12)
        .collect();
    let question: String = above
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(600)
        .collect();
    Some(PromptInfo {
        signature: format!("{question}|{}", options.join("|")),
        cursor,
        question,
        options,
    })
}

/// Parses a numbered menu such as "1. Yes", "> 2. No", with an optional cursor marker on one option.
fn parse_numbered_menu(screen: &str) -> Option<PromptInfo> {
    static LINE: LazyLock<Regex> =
        LazyLock::new(|| re(r"^\s*([❯>›]\s*)?([0-9]{1,2})[.)]\s+([^\r\u{2028}\u{2029}]+?)\s*$"));
    static DECOR: LazyLock<Regex> = LazyLock::new(|| re(r"[│╭╮╰╯─]"));
    let text = tail(screen, 30);
    let lines: Vec<&str> = text.split('\n').collect();
    struct Opt {
        n: u32,
        text: String,
        cur: bool,
        line: usize,
    }
    let opts: Vec<Opt> = lines
        .iter()
        .enumerate()
        .filter_map(|(line, l)| {
            let m = LINE.captures(l)?;
            Some(Opt {
                n: m[2].parse().ok()?,
                text: m[3].to_string(),
                cur: m.get(1).is_some(),
                line,
            })
        })
        .collect();
    // The last run of consecutively numbered options that starts at 1.
    let start = opts.iter().rposition(|o| o.n == 1)?;
    let mut run: Vec<&Opt> = Vec::new();
    for o in &opts[start..] {
        if usize::try_from(o.n).ok() == Some(run.len() + 1) {
            run.push(o);
        } else {
            break;
        }
    }
    if run.len() < 2 || !run.iter().any(|o| o.cur) {
        return None;
    }
    let first = run[0].line;
    let question = (first.saturating_sub(8)..first)
        .rev()
        .map(|i| DECOR.replace_all(lines[i], "").trim().to_string())
        .find(|t| !t.is_empty())
        .unwrap_or_default();
    let options: Vec<String> = run.iter().map(|o| o.text.clone()).collect();
    Some(PromptInfo {
        signature: format!("{question}|{}", options.join("|")),
        cursor: run.iter().position(|o| o.cur).unwrap_or(0),
        question,
        options,
    })
}

/// The tmux keys that move to option `index` and confirm it.
pub fn menu_keys(p: &PromptInfo, index: i64) -> Vec<String> {
    let delta = index - p.cursor as i64;
    let key = if delta >= 0 { "Down" } else { "Up" };
    let mut keys = vec![key.to_string(); delta.unsigned_abs() as usize];
    keys.push("Enter".into());
    keys
}

/// Looks only at the bottom of the screen, so words inside agent output rarely trigger it.
pub fn detect_limit(screen: &str) -> Option<LimitInfo> {
    static LIMIT: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"(?i)(?:you['\u{2019}]ve hit your (session|weekly|usage) limit|(session|5-hour|weekly|usage) limit (?:reached|hit)|limit reached)(?:[^\n]*?resets?\s*(?:at\s*)?([^\n]+))?",
        )
    });
    let t = tail(screen, 10);
    let m = LIMIT.captures(&t)?;
    let word = m
        .get(1)
        .or(m.get(2))
        .map_or(String::new(), |w| w.as_str().to_lowercase());
    Some(LimitInfo {
        kind: if word == "weekly" {
            LimitKind::Weekly
        } else {
            LimitKind::Session
        },
        resets_at: m.get(3).map(|r| r.as_str().trim().to_string()),
    })
}

/// Spots a temporary rate limit message on screen, as opposed to a session or usage limit.
pub fn detect_rate_limit(screen: &str) -> Option<LimitInfo> {
    static RATE: LazyLock<Regex> = LazyLock::new(|| {
        re(r"(?i)rate[- ]limit(ed)?(?-u:\b)[^\n\r\u{2028}\u{2029}]*(retry|try again|exceeded)")
    });
    RATE.is_match(&tail(screen, 6)).then_some(LimitInfo {
        kind: LimitKind::Rate,
        resets_at: None,
    })
}

/// Spots any usage limit message on screen, and says which kind it is and when it resets if the screen shows that.
fn any_limit(screen: &str) -> Option<LimitInfo> {
    detect_limit(screen).or_else(|| detect_rate_limit(screen))
}

impl AdapterId {
    /// The command line that starts this agent, including its model and policy flags.
    pub fn argv(self, c: &LaunchCtx) -> Vec<String> {
        let mut a = vec![self.as_str().to_string()];
        let mut push = |xs: &[&str]| a.extend(xs.iter().map(|s| s.to_string()));
        match self {
            Self::Claude => {
                if let Some(m) = c.model {
                    push(&["--model", m]);
                }
                match c.policy {
                    Policy::Autonomous => push(&["--dangerously-skip-permissions"]),
                    Policy::Plan => push(&["--permission-mode", "plan"]),
                    Policy::Ask => {}
                }
                if let Some(cfg) = c.mcp_config {
                    push(&["--mcp-config", cfg, "--allowedTools"]);
                    for t in [
                        "say",
                        "ask_human",
                        "report",
                        "send_file",
                        "assign",
                        "task_done",
                    ] {
                        a.push(format!("mcp__claudecord__{t}"));
                    }
                }
                a.extend(["-n".into(), c.name.into()]);
            }
            Self::Agy => {
                if let Some(m) = c.model {
                    push(&["--model", m]);
                }
                match c.policy {
                    Policy::Autonomous => push(&["--dangerously-skip-permissions"]),
                    Policy::Plan => push(&["--mode", "plan"]),
                    Policy::Ask => {}
                }
            }
            Self::Codex => {
                push(&["--no-alt-screen"]);
                if let Some(m) = c.model {
                    push(&["-m", m]);
                }
                match c.policy {
                    Policy::Autonomous => push(&["-s", "workspace-write", "-a", "never"]),
                    Policy::Plan => push(&["-s", "read-only", "-a", "on-request"]),
                    Policy::Ask => push(&["-s", "workspace-write", "-a", "on-request"]),
                }
            }
        }
        a
    }

    /// Reads the terminal screen and says what the agent is doing: idle, working, waiting on a prompt, or limited.
    pub fn detect(self, screen: &str) -> ScreenState {
        match self {
            Self::Claude => {
                static MENU_HINT: LazyLock<Regex> = LazyLock::new(|| {
                    re(
                        r"(?i)esc to cancel|enter to select|do you want|do you trust|bypass permissions|select",
                    )
                });
                static BUSY: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)esc to interrupt"));
                static READY: LazyLock<Regex> = LazyLock::new(|| {
                    re(
                        r"(?i)(\? for shortcuts|bypass permissions|plan mode|accept edits|auto mode|❯)",
                    )
                });
                static EXEC: LazyLock<Regex> =
                    LazyLock::new(|| re(r"(?i)(running|⎿\s+\S+\.{3}|\([0-9]+s)"));
                let t = tail(screen, 15);
                let busy = BUSY.is_match(&t);
                let prompt = parse_menu(screen).filter(|_| MENU_HINT.is_match(&tail(screen, 30)));
                let ready = !busy && prompt.is_none() && READY.is_match(&t);
                let executing = busy && EXEC.is_match(&t);
                ScreenState {
                    busy,
                    ready,
                    executing,
                    prompt,
                    limit: any_limit(screen),
                }
            }
            Self::Agy | Self::Codex => {
                static AGY: LazyLock<Regex> =
                    LazyLock::new(|| re(r"(?i)(esc to (interrupt|cancel)|working|thinking)"));
                static CODEX: LazyLock<Regex> =
                    LazyLock::new(|| re(r"(?i)(esc to interrupt|working|thinking)"));
                let t = tail(screen, 8);
                let pattern: &Regex = if self == Self::Agy { &AGY } else { &CODEX };
                let busy = pattern.is_match(&t);
                let prompt = parse_menu(screen);
                ScreenState {
                    busy,
                    ready: !busy && prompt.is_none(),
                    executing: false,
                    prompt,
                    limit: any_limit(screen),
                }
            }
        }
    }

    /// The option to pick by itself for a startup dialog such as folder trust. Like the original it can be -1 when the
    /// dialog is recognised but no option matches, and None when it is not a startup dialog at all.
    pub fn startup_choice(self, p: &PromptInfo) -> Option<i64> {
        static YES: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^yes"));
        static ACCEPT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)yes.*accept"));
        static TRUST: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)trust"));
        static BYPASS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)bypass permissions"));
        let all = format!("{} {}", p.question, p.options.join(" "));
        let find = |r: &Regex| {
            p.options
                .iter()
                .position(|o| r.is_match(o))
                .map_or(-1, |i| i as i64)
        };
        if TRUST.is_match(&all) {
            return Some(find(&YES));
        }
        (self == Self::Claude && BYPASS.is_match(&all)).then(|| find(&ACCEPT))
    }

    /// The keystrokes that choose option `index` in a menu prompt this agent is showing.
    pub fn select_keys(self, p: &PromptInfo, index: i64) -> Vec<String> {
        menu_keys(p, index)
    }

    /// Keys that open a free text entry, for an answer that matches no option. Only Claude has one.
    pub fn other_keys(self, p: &PromptInfo) -> Option<Vec<String>> {
        static OTHER: LazyLock<Regex> =
            LazyLock::new(|| re(r"(?i)^(other|type something|chat about this)"));
        if self != Self::Claude {
            return None;
        }
        p.options
            .iter()
            .position(|o| OTHER.is_match(o))
            .map(|i| menu_keys(p, i as i64))
    }
}
