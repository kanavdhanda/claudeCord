//! Best-effort secret scrubbing for text and files that leave a machine through an agent. A prompt-injected agent
//! can be told to paste credentials into chat, so everything that reaches Discord or a peer passes through here
//! first. It catches well-known token shapes. It is a safety net, not a guarantee.
//!
//! Matching notes, all deliberate:
//! - The `regex` crate has no lookahead. A lookahead (`(?!\[redacted)`) would keep scrubbing twice from mangling a marker. Here the match is made and then skipped if its value is already a marker.
//! - Tokens with a distinctive prefix (ghp_, xox, AKIA, AIza, ccn1.) are caught even when glued to the word before them, since a
//!   secret pasted as `tokenghp_...` is still a secret. Shorter or common prefixes (`sk-`, a bare JWT) keep a word boundary in front
//!   so ordinary words such as `task-...` are left alone.
//! - `\b`, `\w` and `\d` are forced to ASCII with `(?-u:...)`, so word characters mean the same thing on every machine.

use regex::{Captures, Regex};
use std::collections::BTreeSet;
use std::sync::LazyLock;

struct Rule {
    kind: &'static str,
    re: Regex,
    /// High confidence rules also block file uploads.
    strong: bool,
    /// Keep capture group 1 (for example the variable name) and replace only the rest.
    keep: bool,
}

/// Builds one scrubbing rule from a pattern.
fn rule(kind: &'static str, pattern: &str, strong: bool, keep: bool) -> Rule {
    Rule {
        kind,
        re: Regex::new(pattern).expect("redaction pattern"),
        strong,
        keep,
    }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(
            "private key",
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----[\s\S]*?(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY-----|$)",
            true,
            false,
        ),
        rule("aws key", r"(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)", true, false),
        rule(
            "github token",
            r"(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})(?-u:\b)",
            true,
            false,
        ),
        rule(
            "api key",
            r"(?-u:\b)sk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}(?-u:\b)",
            true,
            false,
        ),
        rule(
            "slack token",
            r"xox[abprs]-[A-Za-z0-9-]{10,}(?-u:\b)",
            true,
            false,
        ),
        rule(
            "discord token",
            r"(?-u:\b)[MNO][A-Za-z0-9_-]{23,25}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}(?-u:\b)",
            true,
            false,
        ),
        rule(
            "claudecord token",
            r"ccn1\.[A-Za-z0-9_-]{20,}(?-u:\b)",
            true,
            false,
        ),
        rule("google key", r"AIza[0-9A-Za-z_-]{35}(?-u:\b)", true, false),
        rule(
            "jwt",
            r"(?-u:\b)eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?-u:\b)",
            true,
            false,
        ),
        // Group 1 is the prefix kept, group 2 the value. A value that is already a marker is left alone.
        rule(
            "bearer token",
            r"(?-u:\b)(Bearer\s+)([A-Za-z0-9._~+/-]{20,}=*)",
            false,
            true,
        ),
        rule(
            "secret assignment",
            r#"(?i)(?-u:\b)([A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_?KEY|CREDENTIALS?)[A-Za-z0-9_]*\s*[=:]\s*['"]?)([^\s'",;]{8,})"#,
            false,
            true,
        ),
    ]
});

/// A line that assigns a value to a secret-looking name, such as `export DB_PASSWORD=...`.
static ENV_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)^\s*(?:export\s+)?[A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_?KEY|CREDENTIALS?|ACCESS_?KEY)[A-Za-z0-9_]*\s*[=:]\s*['"]?[^\s'"]{8,}"#,
    )
    .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    /// Kind of secret removed, one entry per match.
    pub found: Vec<String>,
}

/// Replaces every credential-looking value in `text` with a marker and reports what kinds were found. Safe to run twice.
pub fn redact(text: &str) -> Redaction {
    let mut found = Vec::new();
    let mut out = text.to_string();
    for r in RULES.iter() {
        out =
            r.re.replace_all(&out, |c: &Captures| {
                if r.keep
                    && c.get(2)
                        .is_some_and(|v| v.as_str().starts_with("[redacted"))
                {
                    return c[0].to_string();
                }
                found.push(r.kind.to_string());
                let kept = if r.keep {
                    c.get(1).map_or("", |m| m.as_str())
                } else {
                    ""
                };
                format!("{kept}[redacted {}]", r.kind)
            })
            .into_owned();
    }
    Redaction { text: out, found }
}

/// Detects a dump of environment variables or credentials, however the file is named. Renaming `.env` to
/// `notes.md` must not get it past the filename check, so the content is what counts.
pub fn looks_like_env_dump(text: &str) -> bool {
    text.split('\n')
        .take(5000)
        .filter(|l| ENV_LINE.is_match(l))
        .nth(1)
        .is_some()
}

/// Kinds of secret in a file, so it can be blocked: high confidence token shapes, private keys, and files that look
/// like a dump of environment variables. Binary files are not scanned.
pub fn find_secrets_in_file(data: &[u8]) -> Vec<String> {
    if data.iter().take(8192).any(|&b| b == 0) {
        return vec![];
    }
    let text = String::from_utf8_lossy(data);
    let mut kinds = BTreeSet::new();
    if looks_like_env_dump(&text) {
        kinds.insert("environment variable dump");
    }
    for r in RULES.iter().filter(|r| r.strong) {
        if r.re.is_match(&text) {
            kinds.insert(r.kind);
        }
    }
    kinds.into_iter().map(String::from).collect()
}
