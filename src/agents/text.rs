//! Everything pasted into an agent's terminal passes through here. Peer messages, file notices and human replies are
//! text the agent will read as input, so the goal is that no message can pose as someone else or escape the paste.

use crate::jsslice;
use regex::Regex;
use std::sync::LazyLock;

/// Whether a character must never reach a terminal: control codes and bidirectional overrides.
fn is_control(c: char) -> bool {
    matches!(c,
        '\u{0000}'..='\u{0008}' | '\u{000b}'..='\u{001f}' | '\u{007f}'..='\u{009f}'
        | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Removes control characters, keeping newline and tab. A message containing the bracketed paste end marker
/// (ESC [ 2 0 1 ~) would otherwise end the paste early, and whatever followed would be typed as keystrokes. Removing
/// ESC and the C1 range, which includes the single byte CSI, also blocks cursor and screen manipulation.
pub fn strip_control(text: &str) -> String {
    text.chars().filter(|&c| !is_control(c)).collect()
}

/// Quotes every line after the first, so a message can never contain a line that looks like another sender's
/// header such as `[engineer] ...`. Only the first line carries the real header, added by the hub.
/// The SHA-256 of some bytes, as lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn quote_body(text: &str) -> String {
    let clean = strip_control(text);
    let mut lines = clean.split('\n');
    let mut out = lines.next().unwrap_or("").to_string();
    for l in lines {
        out.push_str("\n> ");
        out.push_str(l);
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delivery {
    pub from: String,
    pub text: String,
    pub thread: Option<String>,
    /// Hub delivery id, reported back once the agent starts on it.
    pub msg_id: Option<String>,
}

/// Turns a batch of deliveries into the one text pasted into an agent: a short header per message, bodies quoted.
pub fn format_deliveries(items: &[Delivery]) -> String {
    items
        .iter()
        .map(|d| {
            let thread = d.thread.as_deref().filter(|t| !t.is_empty());
            let thread = thread.map_or(String::new(), |t| {
                format!(" | thread: {}", strip_control(t))
            });
            format!(
                "[{}{thread}] {}",
                strip_control(&d.from),
                quote_body(&d.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Turns a folder name into a valid project name.
pub fn project_slug(name: &str) -> String {
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._-]+").unwrap());
    let s = BAD.replace_all(name, "-");
    let s = s.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let s = s.trim_end_matches('-');
    let s = jsslice(s, 64);
    if s.is_empty() {
        "project".into()
    } else {
        s.into()
    }
}

/// A file name that is safe to create: no directories, no control or path characters, never hidden, at most 120.
pub fn safe_name(name: &str) -> String {
    let base = name.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let replaced: String = base
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let dots = replaced.len() - replaced.trim_start_matches('.').len();
    let replaced = format!("{}{}", "_".repeat(usize::from(dots > 0)), &replaced[dots..]);
    let out = if replaced.is_empty() {
        "file"
    } else {
        &replaced
    };
    jsslice(out, 120).to_string()
}

static SENSITIVE_NAMES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^\.env(\..*)?$",
        r"(?i)^\.(npmrc|netrc|pypirc|git-credentials|htpasswd|pgpass|my\.cnf)$",
        r"(?i)^id_(rsa|dsa|ecdsa|ed25519)(\.pub)?$",
        r"(?i)^(credentials?|secrets?)(\.[a-z0-9]+)?$",
        r"(?i)\.(pem|key|p12|pfx|jks|keystore|kdbx|ovpn|tfstate)$",
        r"(?i)^terraform\.tfvars$",
        r"(?i)^service[-_]?account.*\.json$",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

const SENSITIVE_DIRS: [&str; 5] = [".ssh", ".aws", ".gnupg", ".kube", ".claude-mesh"];

/// Files an agent must never send, even from inside the project. A prompt-injected agent can be told to upload
/// credentials, so the obvious carriers are refused outright. It is a denylist and cannot be complete.
pub fn is_sensitive_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let Some((name, dirs)) = parts.split_last() else {
        return false;
    };
    SENSITIVE_NAMES.iter().any(|re| re.is_match(name))
        || dirs.iter().any(|d| SENSITIVE_DIRS.contains(d))
}
