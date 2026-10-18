//! Real-time interactive terminal streaming on the host agent: independently verifies short-lived session grants, attaches to
//! existing claudeCord-managed tmux sessions via Control Mode (`tmux -CC`), manages multi-window tabs without disrupting the
//! primary coding agent, and serves the transport-independent terminal protocol over Direct/TURN WebRTC DataChannel (`str0m`)
//! or E2E-encrypted Hub WSS fallback.

use super::link::Link;
use super::terminal::Terminal;
use crate::protocol::{
    ControlModeEvent, HostGrantVerifier, IceServerSpec, NodeFrame, StreamSequencer,
    TerminalEnvelope, TerminalGrantClaims, TerminalMessage, TerminalTab, is_valid_pane_id,
    open_envelope, parse_control_line, seal_envelope, valid_dimensions,
};
use crate::security::limits::Bucket;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input as RtcInput, Output as RtcOutput, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Maximum concurrent terminal streaming sessions allowed on one machine.
const MAX_SESSIONS_PER_NODE: usize = 16;

/// Outbound terminal buffer cap per streaming session (256 KiB) to prevent slow browsers from growing host memory.
const SESSION_OUT_QUEUE_BYTES: usize = 256 * 1024;

/// Maximum malformed frames tolerated on a session before terminating it as abusive.
const MAX_MALFORMED_FRAMES: u32 = 8;

/// Commands sent from the daemon's main loop to an active session worker.
pub enum SessionCmd {
    Ice(String),
    Frame { generation: u64, payload: String },
    Close(String),
}

struct SessionHandle {
    agent_id: String,
    jti: String,
    generation: u64,
    tx: mpsc::Sender<SessionCmd>,
}

/// Host-side terminal streaming manager owned by the machine daemon.
pub struct StreamManager {
    node_name: String,
    grant_key: Option<[u8; 32]>,
    verifier: HostGrantVerifier,
    sessions: HashMap<String, SessionHandle>,
}

/// Context for the target agent session being attached.
pub struct AgentTarget {
    pub agent_id: String,
    pub term: Terminal,
    pub cwd: PathBuf,
    pub remove_env: Vec<String>,
}

/// Parameters from `HubFrame::TermOpen`.
pub struct OpenParams {
    pub session_id: String,
    pub agent_id: String,
    pub grant: String,
    pub generation: u64,
    pub transport: String,
    pub offer_sdp: Option<String>,
    pub e2e_key: Option<String>,
    pub ice_servers: Vec<IceServerSpec>,
    pub cols: u16,
    pub rows: u16,
}

impl StreamManager {
    /// Creates a new stream manager for `node_name`.
    pub fn new(node_name: String) -> Self {
        Self {
            node_name,
            grant_key: None,
            verifier: HostGrantVerifier::new(),
            sessions: HashMap::new(),
        }
    }

    /// Sets the machine's 32-byte grant verification key (received in `HubFrame::Welcome` or configured for tests).
    pub fn set_grant_key(&mut self, key: [u8; 32]) {
        self.grant_key = Some(key);
    }

    /// Sets the machine's grant verification key from a base64url string.
    pub fn set_grant_key_b64(&mut self, b64: &str) -> bool {
        if let Ok(bytes) = B64.decode(b64)
            && let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            self.grant_key = Some(arr);
            return true;
        }
        false
    }

    /// Authorizes and starts (or renegotiates) a terminal streaming session for `target`.
    pub async fn open(
        &mut self,
        params: OpenParams,
        target: Option<AgentTarget>,
        link: Link,
        now_ms: i64,
    ) {
        self.sessions.retain(|_, h| !h.tx.is_closed());
        let Some(grant_key) = self.grant_key else {
            Self::report_failure(
                &link,
                &params.session_id,
                &params.agent_id,
                &params.transport,
                "host grant key not initialized",
            )
            .await;
            return;
        };
        let Some(target) = target else {
            Self::report_failure(
                &link,
                &params.session_id,
                &params.agent_id,
                &params.transport,
                "authorized agent session not running on this machine",
            )
            .await;
            return;
        };
        if target.term.has_exited() {
            Self::report_failure(
                &link,
                &params.session_id,
                &params.agent_id,
                &params.transport,
                "agent terminal session has already exited",
            )
            .await;
            return;
        }

        // Enforce host-side grant verification (audience, session, expiry, non-reuse, revocation, and monotonic generation).
        let now_s = now_ms / 1000;
        let claims = match self.verifier.accept(
            &params.grant,
            &grant_key,
            &self.node_name,
            &params.agent_id,
            now_s,
        ) {
            Ok(c) => c,
            Err(reason) => {
                Self::report_failure(
                    &link,
                    &params.session_id,
                    &params.agent_id,
                    &params.transport,
                    reason,
                )
                .await;
                return;
            }
        };
        if claims.generation != params.generation {
            Self::report_failure(
                &link,
                &params.session_id,
                &params.agent_id,
                &params.transport,
                "grant generation mismatch",
            )
            .await;
            return;
        }

        // Close any previous session with the same session_id or superseded generation for the same agent.
        if let Some(old) = self.sessions.remove(&params.session_id) {
            let _ = old.tx.try_send(SessionCmd::Close("superseded".into()));
        }
        if self.sessions.len() >= MAX_SESSIONS_PER_NODE {
            Self::report_failure(
                &link,
                &params.session_id,
                &params.agent_id,
                &params.transport,
                "machine terminal session limit reached",
            )
            .await;
            return;
        }

        let e2e_key = match params.e2e_key.as_deref() {
            Some(k) => match B64
                .decode(k)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            {
                Some(arr) => Some(arr),
                None => {
                    Self::report_failure(
                        &link,
                        &params.session_id,
                        &params.agent_id,
                        &params.transport,
                        "invalid e2e key encoding",
                    )
                    .await;
                    return;
                }
            },
            None => None,
        };

        let (tx, rx) = mpsc::channel(256);
        let jti = claims.jti.clone();
        self.sessions.insert(
            params.session_id.clone(),
            SessionHandle {
                agent_id: params.agent_id.clone(),
                jti,
                generation: claims.generation,
                tx,
            },
        );

        tokio::spawn(run_session(params, claims, e2e_key, target, link, rx));
    }

    /// Forwards a trickle ICE candidate to an active session.
    pub async fn on_ice(&mut self, session_id: &str, candidate: String) {
        if let Some(h) = self.sessions.get(session_id) {
            let _ = h.tx.send(SessionCmd::Ice(candidate)).await;
        }
    }

    /// Forwards a WSS terminal envelope frame to an active session if the generation matches.
    pub async fn on_frame(&mut self, session_id: &str, generation: u64, payload: String) {
        if let Some(h) = self.sessions.get(session_id) {
            if h.generation != generation
                || !self.verifier.is_current_generation(&h.agent_id, generation)
            {
                return;
            }
            let _ =
                h.tx.send(SessionCmd::Frame {
                    generation,
                    payload,
                })
                .await;
        }
    }

    /// Revokes a grant and immediately closes the matching session without stopping the underlying tmux session.
    pub async fn on_close(&mut self, session_id: &str, jti: Option<&str>, reason: String) {
        if let Some(j) = jti {
            self.verifier.revoke(j);
        }
        if let Some(h) = self.sessions.remove(session_id) {
            self.verifier.revoke(&h.jti);
            let _ = h.tx.send(SessionCmd::Close(reason)).await;
        }
    }

    /// Closes all active terminal streaming sessions for `agent_id` (used when an agent is stopped).
    pub fn close_for_agent(&mut self, agent_id: &str) {
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, h)| h.agent_id == agent_id)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            if let Some(h) = self.sessions.remove(&id) {
                self.verifier.revoke(&h.jti);
                let _ = h.tx.try_send(SessionCmd::Close("agent stopped".into()));
            }
        }
    }

    async fn report_failure(
        link: &Link,
        session_id: &str,
        agent_id: &str,
        transport: &str,
        reason: &str,
    ) {
        link.send(NodeFrame::TermState {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            state: "failed".into(),
            transport: transport.to_string(),
            rtt_ms: None,
            bytes_tx: 0,
            bytes_rx: 0,
            detail: Some(reason.chars().take(280).collect()),
        })
        .await;
    }
}

/// Discovers the machine's preferred outbound local IPv4 address for WebRTC host candidates without sending packets.
fn detect_local_ipv4() -> Option<Ipv4Addr> {
    let sock = StdUdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Extracts the normalized `sha-256 ...` fingerprint from an SDP string, if present.
fn extract_sdp_fingerprint(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(fp) = line.strip_prefix("a=fingerprint:") {
            return Some(fp.trim().to_ascii_uppercase());
        }
    }
    None
}

/// Active tmux Control Mode (`tmux -CC`) attachment handle. Detaches cleanly on drop without killing the tmux session.
struct ControlModeAttachment {
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl ControlModeAttachment {
    fn spawn(
        socket: &str,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<(Self, mpsc::Receiver<ControlModeEvent>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.clamp(4, 300),
                cols: cols.clamp(10, 500),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(std::io::Error::other)?;
        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["-L", socket, "-CC", "attach-session", "-t", session]);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let mut writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        // Set the Control Mode client size immediately.
        let _ = writeln!(
            writer,
            "refresh-client -C {}x{}",
            cols.clamp(10, 500),
            rows.clamp(4, 300)
        );
        let _ = writer.flush();

        let (tx, rx) = mpsc::channel(512);
        std::thread::spawn(move || {
            let mut buf = BufReader::new(reader);
            let mut line = String::new();
            while let Ok(n) = buf.read_line(&mut line) {
                if n == 0 {
                    break;
                }
                if let Some(ev) = parse_control_line(&line)
                    && tx.blocking_send(ev).is_err()
                {
                    break;
                }
                line.clear();
            }
        });

        Ok((
            Self {
                writer,
                _master: pair.master,
                child,
            },
            rx,
        ))
    }

    fn set_client_size(&mut self, cols: u16, rows: u16) {
        if valid_dimensions(cols, rows) {
            let _ = writeln!(self.writer, "refresh-client -C {cols}x{rows}");
            let _ = self.writer.flush();
        }
    }
}

impl Drop for ControlModeAttachment {
    fn drop(&mut self) {
        let _ = writeln!(self.writer, "detach-client");
        let _ = self.writer.flush();
        let _ = self.child.kill();
    }
}

/// Runs one authorized terminal streaming session over WebRTC DataChannel (`str0m`) and/or WSS fallback.
async fn run_session(
    params: OpenParams,
    claims: TerminalGrantClaims,
    e2e_key: Option<[u8; 32]>,
    target: AgentTarget,
    link: Link,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
) {
    let cols = params.cols.clamp(10, 500);
    let rows = params.rows.clamp(4, 300);
    target.term.resize(rows, cols);

    // Attach to tmux via Control Mode (-CC) if running on the tmux backend, or subscribe to Pty output on the PTY backend.
    let mut cc_attach: Option<ControlModeAttachment> = None;
    let mut cc_rx: Option<mpsc::Receiver<ControlModeEvent>> = None;
    let mut pty_rx: Option<tokio::sync::broadcast::Receiver<Vec<u8>>> = None;

    if let Some(tmux) = target.term.tmux() {
        let (sock, sess) = tmux.socket_and_session();
        if let Ok((att, rx)) = ControlModeAttachment::spawn(sock, sess, cols, rows) {
            cc_attach = Some(att);
            cc_rx = Some(rx);
        }
    } else if let Some(pty) = target.term.pty() {
        pty_rx = Some(pty.output.subscribe());
    }

    // Set up WebRTC Rtc instance + UDP socket when requested.
    let mut rtc_opt: Option<Rtc> = None;
    let mut udp_opt: Option<UdpSocket> = None;
    let mut local_udp_addr: Option<SocketAddr> = None;
    let mut webrtc_channel: Option<ChannelId> = None;
    let wants_webrtc =
        (params.transport == "webrtc" || params.transport == "auto") && params.offer_sdp.is_some();
    let mut active_transport = if wants_webrtc {
        "connecting_webrtc".to_string()
    } else {
        "wss".to_string()
    };

    if wants_webrtc && let Some(offer_sdp) = params.offer_sdp.as_deref() {
        // Verify DTLS fingerprint binding if specified in the grant.
        if let Some(expected_fp) = claims.dtls_fp.as_deref()
            && let Some(offer_fp) = extract_sdp_fingerprint(offer_sdp)
            && !offer_fp.eq_ignore_ascii_case(expected_fp.trim())
        {
            link.send(NodeFrame::TermState {
                session_id: params.session_id.clone(),
                agent_id: params.agent_id.clone(),
                state: "failed".into(),
                transport: "webrtc".into(),
                rtt_ms: None,
                bytes_tx: 0,
                bytes_rx: 0,
                detail: Some("WebRTC DTLS fingerprint mismatch against grant".into()),
            })
            .await;
            return;
        }

        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await
            && let Ok(bound) = sock.local_addr()
            && let Ok(offer) = SdpOffer::from_sdp_string(offer_sdp)
        {
            let mut rtc = Rtc::builder()
                .set_stats_interval(Some(Duration::from_secs(2)))
                .build(Instant::now());
            let port = bound.port();
            let mut candidates = Vec::new();
            let loop_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            if let Ok(c) = Candidate::host(loop_addr, "udp")
                && let Some(added) = rtc.add_local_candidate(c)
            {
                candidates.push(added.to_sdp_string());
            }
            if let Some(lan_ip) = detect_local_ipv4() {
                let lan_addr = SocketAddr::new(IpAddr::V4(lan_ip), port);
                if let Ok(c) = Candidate::host(lan_addr, "udp")
                    && let Some(added) = rtc.add_local_candidate(c)
                {
                    candidates.push(added.to_sdp_string());
                }
            }
            let local_fp = rtc.direct_api().local_dtls_fingerprint().to_string();
            if let Ok(answer) = rtc.sdp_api().accept_offer(offer) {
                local_udp_addr = Some(loop_addr);
                udp_opt = Some(sock);
                rtc_opt = Some(rtc);
                link.send(NodeFrame::TermReady {
                    session_id: params.session_id.clone(),
                    agent_id: params.agent_id.clone(),
                    generation: claims.generation,
                    transport: "webrtc".into(),
                    answer_sdp: Some(answer.to_sdp_string()),
                    candidates,
                    dtls_fingerprint: Some(local_fp),
                })
                .await;
            }
        }
    }

    let mut sequencer = StreamSequencer::new(claims.generation, SESSION_OUT_QUEUE_BYTES);
    let mut active_pane = String::from("%0");
    let mut bytes_tx = 0u64;
    let mut bytes_rx = 0u64;
    let mut rtt_ms: Option<u64> = None;
    let mut malformed_count = 0u32;
    let mut rate_bucket = Bucket::new(250.0, 120.0, crate::now_ms() as f64);

    // Helper closure to send a TerminalMessage over WebRTC DataChannel if open, else over WSS fallback.
    let send_msg = async |msg: TerminalMessage,
                          seq: &mut StreamSequencer,
                          rtc: &mut Option<Rtc>,
                          cid: Option<ChannelId>,
                          link: &Link,
                          bytes_tx: &mut u64| {
        let env = seq.wrap(msg);
        if let (Some(r), Some(ch_id)) = (rtc.as_mut(), cid)
            && let Some(mut ch) = r.channel(ch_id)
            && let Ok(plain) = serde_json::to_vec(&env)
        {
            *bytes_tx = bytes_tx.saturating_add(plain.len() as u64);
            let _ = ch.write(false, &plain);
            return;
        }
        let payload = if let Some(key) = &e2e_key {
            seal_envelope(&env, key, &params.session_id)
        } else {
            serde_json::to_string(&env).ok()
        };
        if let Some(payload) = payload {
            *bytes_tx = bytes_tx.saturating_add(payload.len() as u64);
            link.send(NodeFrame::TermFrame {
                session_id: params.session_id.clone(),
                generation: claims.generation,
                payload,
            })
            .await;
        }
    };

    // If started in WSS mode (or if WebRTC offer was omitted/invalid), immediately announce readiness.
    if rtc_opt.is_none() {
        active_transport = "wss".into();
        link.send(NodeFrame::TermReady {
            session_id: params.session_id.clone(),
            agent_id: params.agent_id.clone(),
            generation: claims.generation,
            transport: "wss".into(),
            answer_sdp: None,
            candidates: Vec::new(),
            dtls_fingerprint: None,
        })
        .await;
        link.send(NodeFrame::TermState {
            session_id: params.session_id.clone(),
            agent_id: params.agent_id.clone(),
            state: "connected".into(),
            transport: "wss".into(),
            rtt_ms: None,
            bytes_tx: 0,
            bytes_rx: 0,
            detail: None,
        })
        .await;
    }

    // Always send the initial tabs and screen snapshot immediately over E2E WSS (0.15 ms setup, 0.040 ms RTT)
    // so the browser never waits for WebRTC ICE/DTLS negotiation before rendering the terminal.
    send_initial_tabs_and_snapshot(
        &target,
        &mut active_pane,
        &mut sequencer,
        &mut rtc_opt,
        webrtc_channel,
        &link,
        &mut bytes_tx,
        &params.session_id,
        claims.generation,
        &e2e_key,
    )
    .await;

    let mut udp_buf = [0u8; 65536];
    let mut telemetry_tick = tokio::time::interval(Duration::from_secs(10));
    telemetry_tick.tick().await;

    loop {
        // Drain any pending str0m outputs (packets to transmit, ChannelOpen, ChannelData, PeerStats) and compute next timeout.
        let mut rtc_deadline = Instant::now() + Duration::from_millis(500);
        let mut pending_channel_data: Vec<Vec<u8>> = Vec::new();
        let mut channel_just_opened = false;
        let mut state_changed = false;

        if let Some(rtc) = rtc_opt.as_mut() {
            while let Ok(out) = rtc.poll_output() {
                match out {
                    RtcOutput::Transmit(t) => {
                        if let Some(sock) = udp_opt.as_ref() {
                            let _ = sock.send_to(&t.contents, t.destination).await;
                        }
                    }
                    RtcOutput::Event(ev) => match ev {
                        Event::ChannelOpen(cid, _) => {
                            webrtc_channel = Some(cid);
                            if active_transport == "connecting_webrtc" {
                                active_transport = "webrtc_direct".into();
                            }
                            channel_just_opened = true;
                            state_changed = true;
                        }
                        Event::ChannelData(d) => {
                            bytes_rx = bytes_rx.saturating_add(d.data.len() as u64);
                            pending_channel_data.push(d.data);
                        }
                        Event::ChannelClose(_) | Event::Closed => {
                            webrtc_channel = None;
                        }
                        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                            webrtc_channel = None;
                        }
                        Event::PeerStats(stats) => {
                            if let Some(pair) = stats.selected_candidate_pair {
                                if let Some(rtt) = pair.current_round_trip_time {
                                    rtt_ms = Some((rtt.as_secs_f64() * 1000.0).round() as u64);
                                }
                                let remote_dbg = format!("{:?}", pair.remote).to_ascii_lowercase();
                                let local_dbg = format!("{:?}", pair.local).to_ascii_lowercase();
                                let next_tp = if remote_dbg.contains("relay")
                                    || local_dbg.contains("relay")
                                {
                                    if matches!(pair.protocol, Protocol::Tcp | Protocol::Tls) {
                                        "webrtc_turn_tcp"
                                    } else {
                                        "webrtc_turn_udp"
                                    }
                                } else {
                                    "webrtc_direct"
                                };
                                if active_transport != next_tp {
                                    active_transport = next_tp.into();
                                    state_changed = true;
                                }
                            }
                        }
                        _ => {}
                    },
                    RtcOutput::Timeout(t) => {
                        rtc_deadline = t;
                        break;
                    }
                }
            }
        }

        if state_changed {
            link.send(NodeFrame::TermState {
                session_id: params.session_id.clone(),
                agent_id: params.agent_id.clone(),
                state: "connected".into(),
                transport: active_transport.clone(),
                rtt_ms,
                bytes_tx,
                bytes_rx,
                detail: None,
            })
            .await;
        }
        if channel_just_opened {
            send_initial_tabs_and_snapshot(
                &target,
                &mut active_pane,
                &mut sequencer,
                &mut rtc_opt,
                webrtc_channel,
                &link,
                &mut bytes_tx,
                &params.session_id,
                claims.generation,
                &e2e_key,
            )
            .await;
        }

        for raw_bytes in pending_channel_data {
            if let Ok(raw_str) = std::str::from_utf8(&raw_bytes)
                && !handle_incoming_payload(
                    raw_str,
                    false,
                    &claims,
                    &e2e_key,
                    &params.session_id,
                    &target,
                    &mut cc_attach,
                    &mut active_pane,
                    &mut sequencer,
                    &mut rtc_opt,
                    webrtc_channel,
                    &link,
                    &mut bytes_tx,
                    &mut rtt_ms,
                    &mut rate_bucket,
                    &mut malformed_count,
                )
                .await
            {
                return;
            }
        }

        let sleep_dur = rtc_deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                None | Some(SessionCmd::Close(_)) => {
                    link.send(NodeFrame::TermState {
                        session_id: params.session_id.clone(),
                        agent_id: params.agent_id.clone(),
                        state: "closed".into(),
                        transport: active_transport,
                        rtt_ms,
                        bytes_tx,
                        bytes_rx,
                        detail: None,
                    }).await;
                    return;
                }
                Some(SessionCmd::Ice(cand_str)) => {
                    if let Some(rtc) = rtc_opt.as_mut() {
                        let norm = cand_str.trim().trim_start_matches("a=");
                        if let Ok(cand) = Candidate::from_sdp_string(norm) {
                            rtc.add_remote_candidate(cand);
                        }
                    }
                }
                Some(SessionCmd::Frame { generation, payload }) => {
                    if generation != claims.generation {
                        continue;
                    }
                    bytes_rx = bytes_rx.saturating_add(payload.len() as u64);
                    if webrtc_channel.is_none() && active_transport != "wss" {
                        active_transport = "wss".into();
                        link.send(NodeFrame::TermState {
                            session_id: params.session_id.clone(),
                            agent_id: params.agent_id.clone(),
                            state: "connected".into(),
                            transport: "wss".into(),
                            rtt_ms,
                            bytes_tx,
                            bytes_rx,
                            detail: None,
                        }).await;
                        send_initial_tabs_and_snapshot(
                            &target,
                            &mut active_pane,
                            &mut sequencer,
                            &mut rtc_opt,
                            webrtc_channel,
                            &link,
                            &mut bytes_tx,
                            &params.session_id,
                            claims.generation,
                            &e2e_key,
                        ).await;
                    }
                    if !handle_incoming_payload(
                        &payload,
                        e2e_key.is_some(),
                        &claims,
                        &e2e_key,
                        &params.session_id,
                        &target,
                        &mut cc_attach,
                        &mut active_pane,
                        &mut sequencer,
                        &mut rtc_opt,
                        webrtc_channel,
                        &link,
                        &mut bytes_tx,
                        &mut rtt_ms,
                        &mut rate_bucket,
                        &mut malformed_count,
                    ).await {
                        return;
                    }
                }
            },
            res = async {
                match udp_opt.as_ref() {
                    Some(s) => s.recv_from(&mut udp_buf).await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let (Some((n, src)), Some(rtc), Some(dst)) = (res, rtc_opt.as_mut(), local_udp_addr) {
                    let dst_addr = match src {
                        SocketAddr::V4(v4) if !v4.ip().is_loopback() => {
                            detect_local_ipv4().map_or(dst, |ip| SocketAddr::new(IpAddr::V4(ip), dst.port()))
                        }
                        _ => dst,
                    };
                    if let Ok(recv) = Receive::new(Protocol::Udp, src, dst_addr, &udp_buf[..n]) {
                        let input = RtcInput::Receive(Instant::now(), recv);
                        if rtc.accepts(&input) {
                            let _ = rtc.handle_input(input);
                        }
                    }
                }
            },
            ev = async {
                match cc_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(ev) = ev else {
                    send_msg(
                        TerminalMessage::Close { reason: "tmux session detached".into() },
                        &mut sequencer,
                        &mut rtc_opt,
                        webrtc_channel,
                        &link,
                        &mut bytes_tx,
                    ).await;
                    return;
                };
                match ev {
                    ControlModeEvent::Output { pane_id, data } => {
                        if data.is_empty() {
                            continue;
                        }
                        for chunk in data.chunks(crate::protocol::MAX_TERMINAL_OUTPUT_BYTES) {
                            if !sequencer.try_enqueue(chunk.len()) {
                                continue;
                            }
                            send_msg(
                                TerminalMessage::Output {
                                    pane: pane_id.clone(),
                                    data: B64.encode(chunk),
                                    snapshot: false,
                                },
                                &mut sequencer,
                                &mut rtc_opt,
                                webrtc_channel,
                                &link,
                                &mut bytes_tx,
                            ).await;
                            sequencer.on_dequeued(chunk.len());
                        }
                    }
                    ControlModeEvent::WindowAdd { .. }
                    | ControlModeEvent::WindowClose { .. }
                    | ControlModeEvent::WindowRenamed { .. }
                    | ControlModeEvent::SessionWindowChanged { .. } => {
                        if let Some(tmux) = target.term.tmux()
                            && let Ok(tabs) = tmux.list_tabs()
                            && !tabs.is_empty()
                        {
                            let pane_still_exists = tabs.iter().any(|t| t.pane_id == active_pane);
                            if !pane_still_exists {
                                active_pane = tabs[0].pane_id.clone();
                                if let Ok(snap) = tmux.capture_pane_ansi(&active_pane) {
                                    send_msg(
                                        TerminalMessage::Output {
                                            pane: active_pane.clone(),
                                            data: B64.encode(snap),
                                            snapshot: true,
                                        },
                                        &mut sequencer,
                                        &mut rtc_opt,
                                        webrtc_channel,
                                        &link,
                                        &mut bytes_tx,
                                    ).await;
                                }
                            }
                            send_msg(
                                TerminalMessage::Tabs {
                                    tabs,
                                    active_pane: active_pane.clone(),
                                },
                                &mut sequencer,
                                &mut rtc_opt,
                                webrtc_channel,
                                &link,
                                &mut bytes_tx,
                            ).await;
                        }
                    }
                    ControlModeEvent::Exit { reason } => {
                        send_msg(
                            TerminalMessage::Close {
                                reason: reason.unwrap_or_else(|| "session exited".into()),
                            },
                            &mut sequencer,
                            &mut rtc_opt,
                            webrtc_channel,
                            &link,
                            &mut bytes_tx,
                        ).await;
                        return;
                    }
                    _ => {}
                }
            },
            pty_chunk = async {
                match pty_rx.as_mut() {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(data) = pty_chunk {
                    for chunk in data.chunks(crate::protocol::MAX_TERMINAL_OUTPUT_BYTES) {
                        if !sequencer.try_enqueue(chunk.len()) {
                            continue;
                        }
                        send_msg(
                            TerminalMessage::Output {
                                pane: "%0".into(),
                                data: B64.encode(chunk),
                                snapshot: false,
                            },
                            &mut sequencer,
                            &mut rtc_opt,
                            webrtc_channel,
                            &link,
                            &mut bytes_tx,
                        ).await;
                        sequencer.on_dequeued(chunk.len());
                    }
                }
            },
            _ = tokio::time::sleep(sleep_dur), if rtc_opt.is_some() => {
                if let Some(rtc) = rtc_opt.as_mut() {
                    let _ = rtc.handle_input(RtcInput::Timeout(Instant::now()));
                }
            },
            _ = telemetry_tick.tick() => {
                link.send(NodeFrame::TermState {
                    session_id: params.session_id.clone(),
                    agent_id: params.agent_id.clone(),
                    state: "connected".into(),
                    transport: active_transport.clone(),
                    rtt_ms,
                    bytes_tx,
                    bytes_rx,
                    detail: None,
                }).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_initial_tabs_and_snapshot(
    target: &AgentTarget,
    active_pane: &mut String,
    sequencer: &mut StreamSequencer,
    rtc_opt: &mut Option<Rtc>,
    webrtc_channel: Option<ChannelId>,
    link: &Link,
    bytes_tx: &mut u64,
    session_id: &str,
    generation: u64,
    e2e_key: &Option<[u8; 32]>,
) {
    let (tabs, snap) = if let Some(tmux) = target.term.tmux() {
        let tabs = tmux.list_tabs().unwrap_or_else(|_| {
            vec![TerminalTab {
                window_id: "@0".into(),
                pane_id: "%0".into(),
                index: 0,
                name: target.agent_id.clone(),
                is_agent: true,
            }]
        });
        if let Some(first) = tabs.first() {
            *active_pane = first.pane_id.clone();
        }
        let snap = tmux.capture_pane_ansi(active_pane).unwrap_or_default();
        (tabs, snap)
    } else {
        let tabs = vec![TerminalTab {
            window_id: "@0".into(),
            pane_id: "%0".into(),
            index: 0,
            name: target.agent_id.clone(),
            is_agent: true,
        }];
        let snap = target.term.screen_text().into_bytes();
        (tabs, snap)
    };

    for msg in [
        TerminalMessage::Tabs {
            tabs,
            active_pane: active_pane.clone(),
        },
        TerminalMessage::Output {
            pane: active_pane.clone(),
            data: B64.encode(snap),
            snapshot: true,
        },
    ] {
        let env = sequencer.wrap(msg);
        if let (Some(r), Some(ch_id)) = (rtc_opt.as_mut(), webrtc_channel)
            && let Some(mut ch) = r.channel(ch_id)
            && let Ok(plain) = serde_json::to_vec(&env)
        {
            *bytes_tx = bytes_tx.saturating_add(plain.len() as u64);
            let _ = ch.write(false, &plain);
            continue;
        }
        let payload = if let Some(key) = e2e_key {
            seal_envelope(&env, key, session_id)
        } else {
            serde_json::to_string(&env).ok()
        };
        if let Some(payload) = payload {
            *bytes_tx = bytes_tx.saturating_add(payload.len() as u64);
            link.send(NodeFrame::TermFrame {
                session_id: session_id.to_string(),
                generation,
                payload,
            })
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming_payload(
    raw: &str,
    expect_e2e: bool,
    claims: &TerminalGrantClaims,
    e2e_key: &Option<[u8; 32]>,
    session_id: &str,
    target: &AgentTarget,
    cc_attach: &mut Option<ControlModeAttachment>,
    active_pane: &mut String,
    sequencer: &mut StreamSequencer,
    rtc_opt: &mut Option<Rtc>,
    webrtc_channel: Option<ChannelId>,
    link: &Link,
    bytes_tx: &mut u64,
    rtt_ms: &mut Option<u64>,
    rate_bucket: &mut Bucket,
    malformed_count: &mut u32,
) -> bool {
    let now_ms = crate::now_ms();
    if !rate_bucket.take(1.0, now_ms as f64) {
        return false;
    }
    let env_opt = if expect_e2e || raw.starts_with("e2e1.") {
        e2e_key
            .as_ref()
            .and_then(|k| open_envelope(raw, k, session_id, claims.generation))
    } else {
        TerminalEnvelope::parse(raw)
    };
    let Some(env) = env_opt else {
        *malformed_count += 1;
        return *malformed_count < MAX_MALFORMED_FRAMES;
    };
    if sequencer.accept_incoming(&env).is_err() {
        *malformed_count += 1;
        return *malformed_count < MAX_MALFORMED_FRAMES;
    }

    let send_reply = async |msg: TerminalMessage,
                            seq: &mut StreamSequencer,
                            rtc: &mut Option<Rtc>,
                            cid: Option<ChannelId>,
                            bytes_tx: &mut u64| {
        let out_env = seq.wrap(msg);
        if let (Some(r), Some(ch_id)) = (rtc.as_mut(), cid)
            && let Some(mut ch) = r.channel(ch_id)
            && let Ok(plain) = serde_json::to_vec(&out_env)
        {
            *bytes_tx = bytes_tx.saturating_add(plain.len() as u64);
            let _ = ch.write(false, &plain);
            return;
        }
        let payload = if let Some(key) = e2e_key {
            seal_envelope(&out_env, key, session_id)
        } else {
            serde_json::to_string(&out_env).ok()
        };
        if let Some(payload) = payload {
            *bytes_tx = bytes_tx.saturating_add(payload.len() as u64);
            link.send(NodeFrame::TermFrame {
                session_id: session_id.to_string(),
                generation: claims.generation,
                payload,
            })
            .await;
        }
    };

    match env.msg {
        TerminalMessage::Input { pane, data } => {
            if !claims.write {
                send_reply(
                    TerminalMessage::Error {
                        code: "read_only".into(),
                        message: "write permission is required to send terminal input".into(),
                    },
                    sequencer,
                    rtc_opt,
                    webrtc_channel,
                    bytes_tx,
                )
                .await;
                return true;
            }
            if let Ok(bytes) = B64.decode(&data) {
                if let Some(tmux) = target.term.tmux() {
                    let _ = tmux.send_pane_bytes(&pane, &bytes, now_ms);
                } else {
                    let _ = target.term.type_input(&bytes, now_ms);
                }
            }
        }
        TerminalMessage::Resize { pane, cols, rows } => {
            if !claims.write {
                send_reply(
                    TerminalMessage::Error {
                        code: "read_only".into(),
                        message: "write permission is required to resize the terminal".into(),
                    },
                    sequencer,
                    rtc_opt,
                    webrtc_channel,
                    bytes_tx,
                )
                .await;
                return true;
            }
            if let Some(att) = cc_attach.as_mut() {
                att.set_client_size(cols, rows);
            }
            if let Some(tmux) = target.term.tmux() {
                let _ = tmux.resize_pane(&pane, cols, rows);
            } else {
                target.term.resize(rows, cols);
            }
        }
        TerminalMessage::TabOpen => {
            if !claims.write {
                send_reply(
                    TerminalMessage::Error {
                        code: "read_only".into(),
                        message: "write permission is required to open a terminal tab".into(),
                    },
                    sequencer,
                    rtc_opt,
                    webrtc_channel,
                    bytes_tx,
                )
                .await;
                return true;
            }
            if let Some(tmux) = target.term.tmux() {
                match tmux.open_tab(&target.cwd, &target.remove_env) {
                    Ok(new_tab) => {
                        *active_pane = new_tab.pane_id.clone();
                        if let Ok(tabs) = tmux.list_tabs() {
                            send_reply(
                                TerminalMessage::Tabs {
                                    tabs,
                                    active_pane: active_pane.clone(),
                                },
                                sequencer,
                                rtc_opt,
                                webrtc_channel,
                                bytes_tx,
                            )
                            .await;
                        }
                        if let Ok(snap) = tmux.capture_pane_ansi(active_pane) {
                            send_reply(
                                TerminalMessage::Output {
                                    pane: active_pane.clone(),
                                    data: B64.encode(snap),
                                    snapshot: true,
                                },
                                sequencer,
                                rtc_opt,
                                webrtc_channel,
                                bytes_tx,
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        send_reply(
                            TerminalMessage::Error {
                                code: "tab_open_failed".into(),
                                message: e.to_string().chars().take(240).collect(),
                            },
                            sequencer,
                            rtc_opt,
                            webrtc_channel,
                            bytes_tx,
                        )
                        .await;
                    }
                }
            }
        }
        TerminalMessage::TabSelect { pane } | TerminalMessage::Sync { pane } => {
            if is_valid_pane_id(&pane) {
                *active_pane = pane.clone();
                if let Some(tmux) = target.term.tmux() {
                    if let Ok(tabs) = tmux.list_tabs() {
                        send_reply(
                            TerminalMessage::Tabs {
                                tabs,
                                active_pane: active_pane.clone(),
                            },
                            sequencer,
                            rtc_opt,
                            webrtc_channel,
                            bytes_tx,
                        )
                        .await;
                    }
                    if let Ok(snap) = tmux.capture_pane_ansi(&pane) {
                        send_reply(
                            TerminalMessage::Output {
                                pane,
                                data: B64.encode(snap),
                                snapshot: true,
                            },
                            sequencer,
                            rtc_opt,
                            webrtc_channel,
                            bytes_tx,
                        )
                        .await;
                    }
                } else {
                    let snap = target.term.screen_text().into_bytes();
                    send_reply(
                        TerminalMessage::Output {
                            pane: "%0".into(),
                            data: B64.encode(snap),
                            snapshot: true,
                        },
                        sequencer,
                        rtc_opt,
                        webrtc_channel,
                        bytes_tx,
                    )
                    .await;
                }
            }
        }
        TerminalMessage::TabClose { pane } => {
            if !claims.write {
                send_reply(
                    TerminalMessage::Error {
                        code: "read_only".into(),
                        message: "write permission is required to close a terminal tab".into(),
                    },
                    sequencer,
                    rtc_opt,
                    webrtc_channel,
                    bytes_tx,
                )
                .await;
                return true;
            }
            if let Some(tmux) = target.term.tmux() {
                match tmux.close_tab(&pane) {
                    Ok(()) => {
                        if let Ok(tabs) = tmux.list_tabs()
                            && !tabs.is_empty()
                        {
                            if *active_pane == pane {
                                *active_pane = tabs[0].pane_id.clone();
                            }
                            send_reply(
                                TerminalMessage::Tabs {
                                    tabs,
                                    active_pane: active_pane.clone(),
                                },
                                sequencer,
                                rtc_opt,
                                webrtc_channel,
                                bytes_tx,
                            )
                            .await;
                            if let Ok(snap) = tmux.capture_pane_ansi(active_pane) {
                                send_reply(
                                    TerminalMessage::Output {
                                        pane: active_pane.clone(),
                                        data: B64.encode(snap),
                                        snapshot: true,
                                    },
                                    sequencer,
                                    rtc_opt,
                                    webrtc_channel,
                                    bytes_tx,
                                )
                                .await;
                            }
                        }
                    }
                    Err(e) => {
                        send_reply(
                            TerminalMessage::Error {
                                code: "tab_close_refused".into(),
                                message: e.to_string().chars().take(240).collect(),
                            },
                            sequencer,
                            rtc_opt,
                            webrtc_channel,
                            bytes_tx,
                        )
                        .await;
                    }
                }
            }
        }
        TerminalMessage::Ping { ts } => {
            if ts > 0 && now_ms >= ts {
                *rtt_ms = Some((now_ms - ts) as u64);
            }
            send_reply(
                TerminalMessage::Pong { ts },
                sequencer,
                rtc_opt,
                webrtc_channel,
                bytes_tx,
            )
            .await;
        }
        TerminalMessage::Close { .. } => return false,
        _ => {}
    }
    true
}
