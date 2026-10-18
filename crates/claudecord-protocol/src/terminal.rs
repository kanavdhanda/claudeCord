//! The interactive terminal streaming protocol, short-lived session grants, end-to-end payload encryption, and tmux
//! Control Mode (`-CC`) framing parser. Shared by WebRTC DataChannel and Hub WSS fallback so both transports enforce
//! identical framing, bounds, generation ordering, and host-side authorization.

use crate::jslen;
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};

/// Wire version of the terminal streaming envelope.
pub const TERMINAL_PROTOCOL_VERSION: u8 = 1;

/// Maximum raw bytes in one terminal output chunk (64 KiB).
pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 64 * 1024;

/// Maximum raw bytes in one user input message (4 KiB).
pub const MAX_TERMINAL_INPUT_BYTES: usize = 4 * 1024;

/// Maximum serialized JSON length of a `TerminalEnvelope` accepted off the wire.
pub const MAX_TERMINAL_ENVELOPE_CHARS: usize = 100 * 1024;

/// Minimum and maximum terminal dimensions accepted by the host agent.
pub const MIN_TERMINAL_COLS: u16 = 10;
pub const MAX_TERMINAL_COLS: u16 = 500;
pub const MIN_TERMINAL_ROWS: u16 = 4;
pub const MAX_TERMINAL_ROWS: u16 = 300;

/// Maximum number of concurrent windows (tabs) allowed in one managed agent tmux session.
pub const MAX_TERMINAL_TABS: usize = 8;

/// Default lifetime of a newly issued terminal session grant (seconds).
pub const TERMINAL_GRANT_TTL_SECS: i64 = 120;

/// Default per-machine grant signing key derivation prefix when derived from a node secret.
const GRANT_HEADER: &[u8] = br#"{"alg":"HS256","typ":"CCTERM"}"#;

/// Short-lived, audience-bound, single-use grant authorizing a browser session to attach to one agent's tmux session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalGrantClaims {
    /// Unique grant identifier (non-reusable).
    pub jti: String,
    /// Authenticated user or account identifier.
    pub sub: String,
    /// Target machine (`node_name`) audience.
    pub aud: String,
    /// Project slug.
    pub prj: String,
    /// Authorized `agent_id` (`project/name`) whose tmux session is being attached.
    pub agt: String,
    /// Monotonic connection generation ID; reconnects increment this so stale packets are rejected.
    pub generation: u64,
    /// Explicit read permission flag.
    pub read: bool,
    /// Explicit write permission flag (required for input, resize, and tab mutations).
    pub write: bool,
    /// Issued-at Unix timestamp in seconds.
    pub iat: i64,
    /// Expiry Unix timestamp in seconds.
    pub exp: i64,
    /// Optional expected browser DTLS certificate fingerprint (`sha-256 XX:XX:...`) binding signaling to the peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtls_fp: Option<String>,
}

impl TerminalGrantClaims {
    /// Whether the claims satisfy structural and bounds requirements.
    pub fn is_valid(&self) -> bool {
        !self.jti.is_empty()
            && jslen(&self.jti) <= 80
            && !self.sub.is_empty()
            && jslen(&self.sub) <= 120
            && !self.aud.is_empty()
            && jslen(&self.aud) <= 64
            && crate::is_slug(&self.prj)
            && !self.agt.is_empty()
            && jslen(&self.agt) <= 200
            && self.agt.starts_with(&format!("{}/", self.prj))
            && self.generation > 0
            && self.read
            && self.exp > self.iat
            && (self.exp - self.iat) <= 600
            && self
                .dtls_fp
                .as_deref()
                .is_none_or(|f| !f.is_empty() && jslen(f) <= 160)
    }
}

/// Derives a 32-byte terminal grant signing key for a machine `node` from a hub secret.
pub fn derive_node_grant_key(master_secret: &[u8; 32], node: &str) -> [u8; 32] {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(master_secret).expect("hmac accepts any key");
    m.update(b"claudecord-terminal-grant-v1:");
    m.update(node.as_bytes());
    m.finalize().into_bytes().into()
}

/// Signs `TerminalGrantClaims` with the machine's 32-byte `grant_key` using HS256.
pub fn sign_terminal_grant(claims: &TerminalGrantClaims, grant_key: &[u8; 32]) -> Option<String> {
    if !claims.is_valid() {
        return None;
    }
    let head = B64.encode(GRANT_HEADER);
    let body = B64.encode(serde_json::to_vec(claims).ok()?);
    let signing_input = format!("{head}.{body}");
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(grant_key).expect("hmac accepts any key");
    mac.update(signing_input.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());
    Some(format!("{signing_input}.{sig}"))
}

/// Verifies a signed terminal grant token against `grant_key`, expected `node`, `agent_id`, and `now_s`.
pub fn verify_terminal_grant(
    token: &str,
    grant_key: &[u8; 32],
    expected_node: &str,
    expected_agent: &str,
    now_s: i64,
) -> Result<TerminalGrantClaims, &'static str> {
    if token.len() > 4096 {
        return Err("grant token too large");
    }
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed grant token");
    };
    if head != B64.encode(GRANT_HEADER) {
        return Err("invalid grant header");
    }
    let sig_bytes = B64
        .decode(sig)
        .map_err(|_| "invalid grant signature encoding")?;
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(grant_key).expect("hmac accepts any key");
    mac.update(format!("{head}.{body}").as_bytes());
    mac.verify_slice(&sig_bytes)
        .map_err(|_| "grant signature mismatch")?;
    let body_bytes = B64
        .decode(body)
        .map_err(|_| "invalid grant payload encoding")?;
    let claims: TerminalGrantClaims =
        serde_json::from_slice(&body_bytes).map_err(|_| "invalid grant claims")?;
    if !claims.is_valid() {
        return Err("invalid grant claim bounds");
    }
    if claims.aud != expected_node {
        return Err("grant wrong audience");
    }
    if claims.agt != expected_agent {
        return Err("grant wrong session");
    }
    if claims.exp <= now_s {
        return Err("grant expired");
    }
    if claims.iat > now_s + 60 {
        return Err("grant issued in the future");
    }
    Ok(claims)
}

/// Host-agent state enforcing single-use grants, revocation, and monotonic connection generations per agent session.
#[derive(Debug, Default)]
pub struct HostGrantVerifier {
    used_jtis: HashSet<String>,
    revoked_jtis: HashSet<String>,
    latest_generation: HashMap<String, u64>,
}

impl HostGrantVerifier {
    /// Creates an empty host grant verifier.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks a grant ID (`jti`) as revoked so it can never be used or kept alive.
    pub fn revoke(&mut self, jti: &str) {
        self.revoked_jtis.insert(jti.to_string());
    }

    /// Verifies and consumes a grant token for `expected_node` and `expected_agent` at `now_s`.
    pub fn accept(
        &mut self,
        token: &str,
        grant_key: &[u8; 32],
        expected_node: &str,
        expected_agent: &str,
        now_s: i64,
    ) -> Result<TerminalGrantClaims, &'static str> {
        let claims = verify_terminal_grant(token, grant_key, expected_node, expected_agent, now_s)?;
        if self.revoked_jtis.contains(&claims.jti) {
            return Err("grant revoked");
        }
        if self.used_jtis.contains(&claims.jti) {
            return Err("grant already used");
        }
        let last_gen = self
            .latest_generation
            .get(&claims.agt)
            .copied()
            .unwrap_or(0);
        if claims.generation <= last_gen {
            return Err("stale connection generation");
        }
        if self.used_jtis.len() >= 4096 {
            self.used_jtis.clear();
        }
        self.used_jtis.insert(claims.jti.clone());
        self.latest_generation
            .insert(claims.agt.clone(), claims.generation);
        Ok(claims)
    }

    /// Checks whether `generation` is still the active generation for `agent_id`.
    pub fn is_current_generation(&self, agent_id: &str, generation: u64) -> bool {
        self.latest_generation
            .get(agent_id)
            .is_some_and(|g| *g == generation)
    }
}

/// Metadata for one window (tab) inside an agent's tmux session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalTab {
    /// Stable tmux window identifier (e.g. `@0`, `@1`).
    pub window_id: String,
    /// Stable tmux pane identifier (e.g. `%0`, `%1`).
    pub pane_id: String,
    /// Window index within the session (`0` is always the primary coding agent).
    pub index: u16,
    /// Window title (e.g. `claude`, `zsh`, `bash`).
    pub name: String,
    /// True if this window is the primary coding agent (`index == 0`).
    pub is_agent: bool,
}

impl TerminalTab {
    /// Validates bounds of tab metadata.
    pub fn is_valid(&self) -> bool {
        is_valid_window_id(&self.window_id)
            && is_valid_pane_id(&self.pane_id)
            && self.index <= 64
            && !self.name.is_empty()
            && jslen(&self.name) <= 80
    }
}

/// Validates a tmux pane ID (`%` followed by 1..=8 ASCII digits).
pub fn is_valid_pane_id(pane: &str) -> bool {
    pane.strip_prefix('%')
        .is_some_and(|d| (1..=8).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Validates a tmux window ID (`@` followed by 1..=8 ASCII digits).
pub fn is_valid_window_id(win: &str) -> bool {
    win.strip_prefix('@')
        .is_some_and(|d| (1..=8).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Validates terminal dimensions against protocol bounds.
pub fn valid_dimensions(cols: u16, rows: u16) -> bool {
    (MIN_TERMINAL_COLS..=MAX_TERMINAL_COLS).contains(&cols)
        && (MIN_TERMINAL_ROWS..=MAX_TERMINAL_ROWS).contains(&rows)
}

/// Typed operations inside a `TerminalEnvelope`. Never carries raw shell commands or arbitrary tmux commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TerminalMessage {
    /// Terminal output bytes for `pane` (base64url-encoded). `snapshot` is true when replacing screen state on attach/switch.
    #[serde(rename_all = "camelCase")]
    Output {
        pane: String,
        data: String,
        #[serde(default)]
        snapshot: bool,
    },
    /// User keyboard input bytes for `pane` (base64url-encoded). Requires `write` permission.
    #[serde(rename_all = "camelCase")]
    Input { pane: String, data: String },
    /// Terminal resize event. Requires `write` permission and valid dimensions.
    #[serde(rename_all = "camelCase")]
    Resize { pane: String, cols: u16, rows: u16 },
    /// Active tabs/windows in the tmux session and which pane is currently selected.
    #[serde(rename_all = "camelCase")]
    Tabs {
        tabs: Vec<TerminalTab>,
        active_pane: String,
    },
    /// Opens a new interactive shell tab alongside the coding agent in the project's working directory.
    #[serde(rename_all = "camelCase")]
    TabOpen,
    /// Switches the viewed tab to `pane` and requests its current screen snapshot.
    #[serde(rename_all = "camelCase")]
    TabSelect { pane: String },
    /// Closes a secondary shell tab (`pane`). Refused by the host agent for window 0 (`%0` / the coding agent).
    #[serde(rename_all = "camelCase")]
    TabClose { pane: String },
    /// Requests a fresh screen snapshot for `pane`.
    #[serde(rename_all = "camelCase")]
    Sync { pane: String },
    /// Application-level heartbeat probe (`ts` in ms).
    #[serde(rename_all = "camelCase")]
    Ping { ts: i64 },
    /// Application-level heartbeat reply (`ts` echoed from `Ping`).
    #[serde(rename_all = "camelCase")]
    Pong { ts: i64 },
    /// Clean session close notification.
    #[serde(rename_all = "camelCase")]
    Close { reason: String },
    /// Sanitized protocol or authorization error.
    #[serde(rename_all = "camelCase")]
    Error { code: String, message: String },
}

/// Transport-independent terminal frame shared across WebRTC DataChannel and WSS fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEnvelope {
    /// Protocol version (`TERMINAL_PROTOCOL_VERSION = 1`).
    pub v: u8,
    /// Connection generation ID bound to the grant.
    pub generation: u64,
    /// Monotonic sequence number per direction (1, 2, 3...) for duplicate and gap detection.
    pub seq: u64,
    /// Typed terminal payload.
    pub msg: TerminalMessage,
}

impl TerminalEnvelope {
    /// Checks all size, format, and dimension invariants before processing.
    pub fn is_valid(&self) -> bool {
        if self.v != TERMINAL_PROTOCOL_VERSION || self.generation == 0 || self.seq == 0 {
            return false;
        }
        match &self.msg {
            TerminalMessage::Output { pane, data, .. } => {
                is_valid_pane_id(pane)
                    && data.len() <= (MAX_TERMINAL_OUTPUT_BYTES * 4).div_ceil(3) + 8
                    && B64
                        .decode(data)
                        .is_ok_and(|b| b.len() <= MAX_TERMINAL_OUTPUT_BYTES)
            }
            TerminalMessage::Input { pane, data } => {
                is_valid_pane_id(pane)
                    && !data.is_empty()
                    && data.len() <= (MAX_TERMINAL_INPUT_BYTES * 4).div_ceil(3) + 8
                    && B64
                        .decode(data)
                        .is_ok_and(|b| !b.is_empty() && b.len() <= MAX_TERMINAL_INPUT_BYTES)
            }
            TerminalMessage::Resize { pane, cols, rows } => {
                is_valid_pane_id(pane) && valid_dimensions(*cols, *rows)
            }
            TerminalMessage::Tabs { tabs, active_pane } => {
                !tabs.is_empty()
                    && tabs.len() <= MAX_TERMINAL_TABS
                    && is_valid_pane_id(active_pane)
                    && tabs.iter().all(TerminalTab::is_valid)
            }
            TerminalMessage::TabOpen => true,
            TerminalMessage::TabSelect { pane }
            | TerminalMessage::TabClose { pane }
            | TerminalMessage::Sync { pane } => is_valid_pane_id(pane),
            TerminalMessage::Ping { ts } | TerminalMessage::Pong { ts } => *ts >= 0,
            TerminalMessage::Close { reason } => jslen(reason) <= 200,
            TerminalMessage::Error { code, message } => {
                !code.is_empty()
                    && jslen(code) <= 40
                    && !message.is_empty()
                    && jslen(message) <= 300
            }
        }
    }

    /// Parses and validates a `TerminalEnvelope` from raw JSON, checking the wire length before allocating.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_TERMINAL_ENVELOPE_CHARS {
            return None;
        }
        serde_json::from_str::<Self>(raw)
            .ok()
            .filter(Self::is_valid)
    }
}

/// Associated data binding an E2E-encrypted terminal envelope to its session ID and generation.
fn e2e_aad(session_id: &str, generation: u64) -> Vec<u8> {
    format!("claudecord-term-e2e:{session_id}:{generation}").into_bytes()
}

/// Encrypts a `TerminalEnvelope` using AES-256-GCM so an untrusted WSS relay hub cannot read terminal bytes.
pub fn seal_envelope(env: &TerminalEnvelope, key: &[u8; 32], session_id: &str) -> Option<String> {
    if !env.is_valid() {
        return None;
    }
    let plain = serde_json::to_vec(env).ok()?;
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let aad = e2e_aad(session_id, env.generation);
    let ct = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .ok()?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Some(format!("e2e1.{}", B64.encode(out)))
}

/// Opens and validates an AES-256-GCM sealed `TerminalEnvelope` for `session_id` and `expected_generation`.
pub fn open_envelope(
    sealed: &str,
    key: &[u8; 32],
    session_id: &str,
    expected_generation: u64,
) -> Option<TerminalEnvelope> {
    if sealed.len() > MAX_TERMINAL_ENVELOPE_CHARS * 2 {
        return None;
    }
    let b64 = sealed.strip_prefix("e2e1.")?;
    let raw = B64.decode(b64).ok()?;
    if raw.len() < 12 + 16 {
        return None;
    }
    let (nonce, ct) = raw.split_at(12);
    let aad = e2e_aad(session_id, expected_generation);
    let plain = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key))
        .decrypt(
            &Nonce::try_from(nonce).ok()?,
            Payload { msg: ct, aad: &aad },
        )
        .ok()?;
    let plain_str = std::str::from_utf8(&plain).ok()?;
    let env = TerminalEnvelope::parse(plain_str)?;
    (env.generation == expected_generation).then_some(env)
}

/// Tracks sequence numbers and bounded output queue backpressure for one active terminal stream.
#[derive(Debug)]
pub struct StreamSequencer {
    pub generation: u64,
    next_out_seq: u64,
    last_in_seq: u64,
    max_queue_bytes: usize,
    queued_bytes: usize,
}

impl StreamSequencer {
    /// Creates a sequencer for `generation` with `max_queue_bytes` output backpressure cap.
    pub fn new(generation: u64, max_queue_bytes: usize) -> Self {
        Self {
            generation,
            next_out_seq: 1,
            last_in_seq: 0,
            max_queue_bytes,
            queued_bytes: 0,
        }
    }

    /// Wraps an outgoing `TerminalMessage` with the next sequence number.
    pub fn wrap(&mut self, msg: TerminalMessage) -> TerminalEnvelope {
        let seq = self.next_out_seq;
        self.next_out_seq += 1;
        TerminalEnvelope {
            v: TERMINAL_PROTOCOL_VERSION,
            generation: self.generation,
            seq,
            msg,
        }
    }

    /// Validates that an incoming `TerminalEnvelope` belongs to this generation and is strictly newer than `last_in_seq` (no duplicates/replays).
    pub fn accept_incoming(&mut self, env: &TerminalEnvelope) -> Result<(), &'static str> {
        if !env.is_valid() {
            return Err("invalid envelope");
        }
        if env.generation != self.generation {
            return Err("stale generation");
        }
        if env.seq <= self.last_in_seq {
            return Err("duplicate or out-of-order sequence");
        }
        self.last_in_seq = env.seq;
        Ok(())
    }

    /// Attempts to reserve `bytes` in the outbound buffer; returns false when backpressure limit is reached.
    pub fn try_enqueue(&mut self, bytes: usize) -> bool {
        if self.queued_bytes.saturating_add(bytes) > self.max_queue_bytes {
            return false;
        }
        self.queued_bytes += bytes;
        true
    }

    /// Releases `bytes` from the outbound buffer once transmitted.
    pub fn on_dequeued(&mut self, bytes: usize) {
        self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
    }

    /// Current bytes held in the outbound queue.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }
}

/// Structured notifications parsed from tmux Control Mode (`tmux -CC attach-session -t <session>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlModeEvent {
    /// `%output %<pane_id> <unescaped-bytes>` or `%extended-output %<pane_id> ... : <unescaped-bytes>`.
    Output { pane_id: String, data: Vec<u8> },
    /// `%window-add @<window_id>`.
    WindowAdd { window_id: String },
    /// `%window-close @<window_id>` or `%unlinked-window-close @<window_id>`.
    WindowClose { window_id: String },
    /// `%window-renamed @<window_id> <name>`.
    WindowRenamed { window_id: String, name: String },
    /// `%session-window-changed $<session_id> @<window_id>`.
    SessionWindowChanged { window_id: String },
    /// `%layout-change @<window_id> ...`.
    LayoutChange { window_id: String },
    /// `%begin <time> <num> <flags>`.
    CommandBegin { id: u64 },
    /// `%end <time> <num> <flags>`.
    CommandEnd { id: u64 },
    /// `%error <time> <num> <flags>`.
    CommandError { id: u64 },
    /// `%exit [reason]`.
    Exit { reason: Option<String> },
    /// A line printed inside a `%begin` ... `%end` block (such as `CCWIN\t...` from `list-windows`).
    DataLine(String),
}

/// Unescapes octal sequences (`\ooo` where `o` is `0..=7`) emitted by tmux Control Mode `%output`.
pub fn unescape_tmux_output(escaped: &str) -> Vec<u8> {
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && (b'0'..=b'7').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let val =
                ((bytes[i + 1] - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
            out.push(val);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Parses one line from `tmux -CC` stdout into a structured `ControlModeEvent`.
pub fn parse_control_line(raw_line: &str) -> Option<ControlModeEvent> {
    // Strip trailing CR/LF and optional leading DCS entry `\x1bP1000p` or trailing ST `\x1b\\`.
    let line = raw_line.trim_end_matches(['\r', '\n']);
    let line = line.strip_prefix("\x1bP1000p").unwrap_or(line);
    let line = line.strip_suffix("\x1b\\").unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix("%output ") {
        let (pane_id, payload) = rest.split_once(' ')?;
        if !is_valid_pane_id(pane_id) {
            return None;
        }
        return Some(ControlModeEvent::Output {
            pane_id: pane_id.to_string(),
            data: unescape_tmux_output(payload),
        });
    }
    if let Some(rest) = line.strip_prefix("%extended-output ") {
        let (pane_id, after_pane) = rest.split_once(' ')?;
        if !is_valid_pane_id(pane_id) {
            return None;
        }
        let (_, payload) = after_pane.split_once(" : ")?;
        return Some(ControlModeEvent::Output {
            pane_id: pane_id.to_string(),
            data: unescape_tmux_output(payload),
        });
    }
    if let Some(win) = line.strip_prefix("%window-add ") {
        let win = win.trim();
        return is_valid_window_id(win).then(|| ControlModeEvent::WindowAdd {
            window_id: win.to_string(),
        });
    }
    if let Some(win) = line
        .strip_prefix("%window-close ")
        .or_else(|| line.strip_prefix("%unlinked-window-close "))
    {
        let win = win.trim();
        return is_valid_window_id(win).then(|| ControlModeEvent::WindowClose {
            window_id: win.to_string(),
        });
    }
    if let Some(rest) = line
        .strip_prefix("%window-renamed ")
        .or_else(|| line.strip_prefix("%unlinked-window-renamed "))
    {
        let (win, name) = rest.split_once(' ')?;
        return is_valid_window_id(win).then(|| ControlModeEvent::WindowRenamed {
            window_id: win.to_string(),
            name: name.to_string(),
        });
    }
    if let Some(rest) = line.strip_prefix("%session-window-changed ") {
        let mut parts = rest.split_whitespace();
        let _sess = parts.next()?;
        let win = parts.next()?;
        return is_valid_window_id(win).then(|| ControlModeEvent::SessionWindowChanged {
            window_id: win.to_string(),
        });
    }
    if let Some(rest) = line.strip_prefix("%layout-change ") {
        let win = rest.split_whitespace().next()?;
        return is_valid_window_id(win).then(|| ControlModeEvent::LayoutChange {
            window_id: win.to_string(),
        });
    }
    if let Some(rest) = line.strip_prefix("%begin ") {
        let id = rest.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        return Some(ControlModeEvent::CommandBegin { id });
    }
    if let Some(rest) = line.strip_prefix("%end ") {
        let id = rest.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        return Some(ControlModeEvent::CommandEnd { id });
    }
    if let Some(rest) = line.strip_prefix("%error ") {
        let id = rest.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        return Some(ControlModeEvent::CommandError { id });
    }
    if let Some(rest) = line.strip_prefix("%exit") {
        let reason = rest.trim();
        return Some(ControlModeEvent::Exit {
            reason: (!reason.is_empty()).then(|| reason.to_string()),
        });
    }
    if line.starts_with('%') {
        return None;
    }
    Some(ControlModeEvent::DataLine(line.to_string()))
}

/// Formats raw input bytes into a safe `send-keys -H -t %<pane>` command line for tmux Control Mode stdin,
/// avoiding all shell or string escaping issues by encoding every byte as two hexadecimal digits.
pub fn format_control_send_keys_hex(pane_id: &str, bytes: &[u8]) -> Option<String> {
    if !is_valid_pane_id(pane_id) || bytes.is_empty() || bytes.len() > MAX_TERMINAL_INPUT_BYTES {
        return None;
    }
    let mut cmd = format!("send-keys -H -t {pane_id}");
    for b in bytes {
        cmd.push_str(&format!(" {b:02x}"));
    }
    cmd.push('\n');
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> TerminalGrantClaims {
        TerminalGrantClaims {
            jti: "grant-123".into(),
            sub: "user-1".into(),
            aud: "dgx-node-1".into(),
            prj: "demo".into(),
            agt: "demo/claude".into(),
            generation: 1,
            read: true,
            write: true,
            iat: 1_000,
            exp: 1_120,
            dtls_fp: Some("sha-256 AA:BB:CC".into()),
        }
    }

    #[test]
    fn grant_signing_verification_and_replay_rejection() {
        let master = [7u8; 32];
        let key = derive_node_grant_key(&master, "dgx-node-1");
        let claims = sample_claims();
        let token = sign_terminal_grant(&claims, &key).expect("signs valid claims");

        // Wrong audience, wrong session, or expired timestamps are rejected.
        assert_eq!(
            verify_terminal_grant(&token, &key, "other-node", "demo/claude", 1_050),
            Err("grant wrong audience")
        );
        assert_eq!(
            verify_terminal_grant(&token, &key, "dgx-node-1", "demo/other", 1_050),
            Err("grant wrong session")
        );
        assert_eq!(
            verify_terminal_grant(&token, &key, "dgx-node-1", "demo/claude", 1_200),
            Err("grant expired")
        );

        // HostGrantVerifier accepts once, then rejects replay and stale generation.
        let mut verifier = HostGrantVerifier::new();
        let verified = verifier
            .accept(&token, &key, "dgx-node-1", "demo/claude", 1_050)
            .expect("first accept succeeds");
        assert_eq!(verified, claims);
        assert_eq!(
            verifier.accept(&token, &key, "dgx-node-1", "demo/claude", 1_050),
            Err("grant already used")
        );

        // Stale generation is rejected even with a fresh jti.
        let mut stale = sample_claims();
        stale.jti = "grant-124".into();
        stale.generation = 1;
        let stale_tok = sign_terminal_grant(&stale, &key).unwrap();
        assert_eq!(
            verifier.accept(&stale_tok, &key, "dgx-node-1", "demo/claude", 1_050),
            Err("stale connection generation")
        );

        // Revoked grant is rejected immediately.
        let mut rev = sample_claims();
        rev.jti = "grant-125".into();
        rev.generation = 2;
        let rev_tok = sign_terminal_grant(&rev, &key).unwrap();
        verifier.revoke("grant-125");
        assert_eq!(
            verifier.accept(&rev_tok, &key, "dgx-node-1", "demo/claude", 1_050),
            Err("grant revoked")
        );
    }

    #[test]
    fn envelope_e2e_encryption_and_sequencer_bounds() {
        let mut seq = StreamSequencer::new(3, 1024);
        let env = seq.wrap(TerminalMessage::Input {
            pane: "%0".into(),
            data: B64.encode(b"ls -la\r"),
        });
        let key = [42u8; 32];
        let sealed = seal_envelope(&env, &key, "sess-1").expect("seals valid envelope");
        assert!(!sealed.contains("ls -la"));

        // Wrong session or wrong generation fails AAD check.
        assert!(open_envelope(&sealed, &key, "sess-2", 3).is_none());
        assert!(open_envelope(&sealed, &key, "sess-1", 2).is_none());

        let opened = open_envelope(&sealed, &key, "sess-1", 3).expect("opens with matching AAD");
        assert_eq!(opened, env);

        let mut rx_seq = StreamSequencer::new(3, 1024);
        assert!(rx_seq.accept_incoming(&opened).is_ok());
        assert_eq!(
            rx_seq.accept_incoming(&opened),
            Err("duplicate or out-of-order sequence")
        );

        // Backpressure check.
        assert!(rx_seq.try_enqueue(512));
        assert!(rx_seq.try_enqueue(512));
        assert!(!rx_seq.try_enqueue(1));
        rx_seq.on_dequeued(512);
        assert!(rx_seq.try_enqueue(256));
    }

    #[test]
    fn tmux_control_mode_parser_handles_dcs_octal_and_windows() {
        assert_eq!(
            parse_control_line("\x1bP1000p%begin 1792315482 314 0\r\n"),
            Some(ControlModeEvent::CommandBegin { id: 314 })
        );
        assert_eq!(
            parse_control_line("%output %0 hello\\015\\012\\134world\r\n"),
            Some(ControlModeEvent::Output {
                pane_id: "%0".into(),
                data: b"hello\r\n\\world".to_vec(),
            })
        );
        assert_eq!(
            parse_control_line("%extended-output %1 0 : prompt\\040$\\040\r\n"),
            Some(ControlModeEvent::Output {
                pane_id: "%1".into(),
                data: b"prompt $ ".to_vec(),
            })
        );
        assert_eq!(
            parse_control_line("%window-add @1\r\n"),
            Some(ControlModeEvent::WindowAdd {
                window_id: "@1".into()
            })
        );
        assert_eq!(
            parse_control_line("%window-close @1\r\n"),
            Some(ControlModeEvent::WindowClose {
                window_id: "@1".into()
            })
        );
        assert_eq!(
            format_control_send_keys_hex("%0", b"\x04\r"),
            Some("send-keys -H -t %0 04 0d\n".into())
        );
        assert!(format_control_send_keys_hex("bad;rm", b"a").is_none());
    }
}
