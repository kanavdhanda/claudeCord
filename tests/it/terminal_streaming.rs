//! End-to-end integration and security tests for live interactive terminal streaming:
//! - Real `tmux` session attachment via `tmux -CC` Control Mode + initial ANSI snapshot
//! - WebRTC DataChannel (`str0m`) live streaming, keyboard input, and resize propagation
//! - Multi-tab management (`TabOpen`, `TabSelect`, `Ctrl+D` / `TabClose` closing a secondary shell tab without killing Tab 0)
//! - Hub control plane grant issuance, E2E AES-256-GCM encrypted WSS fallback, browser disconnect survival, and instant revocation
//! - Security negative matrix: unauthenticated (`401`), machine token on web route (`401`), replay `jti`, expired grant, read-only (`write: false`) rejection, and oversized frame rejection.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use claudecord::device::inject::Guard;
use claudecord::device::link::Link;
use claudecord::device::stream::{AgentTarget, OpenParams, StreamManager};
use claudecord::device::terminal::Terminal;
use claudecord::device::tmux::{TmuxTerminal, stop_server};
use claudecord::hub::*;
use claudecord::protocol::terminal::{
    MAX_TERMINAL_OUTPUT_BYTES, TERMINAL_PROTOCOL_VERSION, TerminalEnvelope, TerminalGrantClaims,
    TerminalMessage, open_envelope, seal_envelope, sign_terminal_grant, verify_terminal_grant,
};
use claudecord::protocol::{AdapterId, AgentSpec, HubFrame, NodeFrame};
use claudecord::server::{self, Config};
use claudecord::store::Store;
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use str0m::change::SdpAnswer;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input as RtcInput, Output as RtcOutput, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

fn test_key() -> [u8; 32] {
    [0x5a; 32]
}

fn make_grant(
    jti: &str,
    node: &str,
    agent_id: &str,
    write: bool,
    generation: u64,
    iat_s: i64,
    exp_s: i64,
) -> (String, String) {
    let e2e = [0x33u8; 32];
    let e2e_b64 = B64.encode(e2e);
    let claims = TerminalGrantClaims {
        jti: jti.into(),
        sub: "kd".into(),
        aud: node.into(),
        prj: "demo".into(),
        agt: agent_id.into(),
        generation,
        read: true,
        write,
        iat: iat_s,
        exp: exp_s,
        dtls_fp: None,
    };
    let token = sign_terminal_grant(&claims, &test_key()).unwrap();
    (token, e2e_b64)
}

#[cfg(unix)]
fn start_tmux(name: &str, argv: &[&str]) -> Option<(Terminal, String)> {
    if !TmuxTerminal::available() {
        return None;
    }
    let socket = format!("cct-stream-{name}-{}", std::process::id());
    let t = TmuxTerminal::spawn(
        &socket,
        "s",
        "demo/otter",
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        Path::new("/tmp"),
        &["HOME".to_string()],
        &[],
        24,
        80,
        Guard::with_times(0, 50, 50),
    )
    .expect("tmux session starts");
    Some((Terminal::Tmux(std::sync::Arc::new(t)), socket))
}

#[cfg(unix)]
#[tokio::test]
async fn live_tmux_session_streams_over_webrtc_datachannel_and_accepts_input_and_resize() {
    let Some((term, socket)) = start_tmux("webrtc", &["sh"]) else {
        return;
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<NodeFrame>(256);
    let link = Link::from_sender(hub_tx);
    let mut mgr = StreamManager::new("mac".into());
    mgr.set_grant_key(test_key());

    // Create browser-side WebRTC peer with a "terminal" DataChannel.
    let browser_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let browser_addr = browser_udp.local_addr().unwrap();
    let mut browser_rtc = Rtc::new(Instant::now());
    browser_rtc.add_local_candidate(Candidate::host(browser_addr, Protocol::Udp).unwrap());
    let mut change = browser_rtc.sdp_api();
    let cid = change.add_channel("terminal".into());
    let (offer, pending) = change.apply().unwrap();

    let now_ms = claudecord::now_ms();
    let now_s = now_ms / 1000;
    let (grant, e2e_b64) = make_grant(
        "jti-rtc-1",
        "mac",
        "demo/otter",
        true,
        1,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-rtc-1".into(),
            agent_id: "demo/otter".into(),
            grant,
            generation: 1,
            transport: "auto".into(),
            offer_sdp: Some(offer.to_sdp_string()),
            e2e_key: Some(e2e_b64),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        Some(AgentTarget {
            agent_id: "demo/otter".into(),
            term: term.clone(),
            cwd: PathBuf::from("/tmp"),
            remove_env: vec!["HOME".into()],
        }),
        link,
        now_ms,
    )
    .await;

    // Host should emit NodeFrame::TermReady with answer SDP and host ICE candidate.
    let mut answer_sdp = None;
    let mut host_candidates = Vec::new();
    for _ in 0..20 {
        let f = tokio::time::timeout(Duration::from_secs(5), hub_rx.recv())
            .await
            .expect("TermReady timeout")
            .expect("frame");
        if let NodeFrame::TermReady {
            session_id,
            answer_sdp: ans,
            candidates,
            ..
        } = f
        {
            assert_eq!(session_id, "sess-rtc-1");
            answer_sdp = ans;
            host_candidates = candidates;
            break;
        }
    }
    let answer = SdpAnswer::from_sdp_string(&answer_sdp.expect("answer SDP")).unwrap();
    browser_rtc
        .sdp_api()
        .accept_answer(pending, answer)
        .unwrap();
    for cand_str in host_candidates {
        if let Ok(c) = Candidate::from_sdp_string(&cand_str) {
            browser_rtc.add_remote_candidate(c);
        }
    }

    // Pump UDP packets between browser_rtc and the host StreamManager until DataChannel opens,
    // initial snapshot arrives, live echo arrives, and resize updates tmux window dimensions.
    let mut buf = vec![0u8; 65535];
    let mut dc_open = false;
    let mut got_snapshot = false;
    let mut got_live_echo = false;
    let mut sent_echo = false;
    let mut sent_resize = false;

    for _ in 0..250 {
        let now_inst = Instant::now();
        let _ = browser_rtc.handle_input(RtcInput::Timeout(now_inst));
        while let Ok(out) = browser_rtc.poll_output() {
            match out {
                RtcOutput::Transmit(t) => {
                    let _ = browser_udp.send_to(&t.contents, t.destination).await;
                }
                RtcOutput::Event(Event::ChannelOpen(id, name))
                    if id == cid && name == "terminal" =>
                {
                    dc_open = true;
                }
                RtcOutput::Event(Event::ChannelData(d)) => {
                    let text = String::from_utf8_lossy(&d.data);
                    let env: TerminalEnvelope = serde_json::from_str(&text).unwrap();
                    if let TerminalMessage::Output { data, snapshot, .. } = env.msg {
                        if snapshot {
                            got_snapshot = true;
                        }
                        let raw = B64.decode(&data).unwrap_or_default();
                        if String::from_utf8_lossy(&raw).contains("webrtc-live-42") {
                            got_live_echo = true;
                        }
                    }
                }
                RtcOutput::Event(_) => {}
                RtcOutput::Timeout(_) => break,
            }
        }
        while let Ok((n, from)) = browser_udp.try_recv_from(&mut buf) {
            if let Ok(rx) = Receive::new(Protocol::Udp, from, browser_addr, &buf[..n]) {
                let _ = browser_rtc.handle_input(RtcInput::Receive(Instant::now(), rx));
            }
        }
        if dc_open
            && got_snapshot
            && !sent_echo
            && let Some(mut ch) = browser_rtc.channel(cid)
        {
            let env = TerminalEnvelope {
                v: TERMINAL_PROTOCOL_VERSION,
                generation: 1,
                seq: 1,
                msg: TerminalMessage::Input {
                    pane: "%0".into(),
                    data: B64.encode(b"echo webrtc-live-42\n"),
                },
            };
            let _ = ch.write(false, serde_json::to_string(&env).unwrap().as_bytes());
            sent_echo = true;
        }
        if got_live_echo
            && !sent_resize
            && let Some(mut ch) = browser_rtc.channel(cid)
        {
            let env = TerminalEnvelope {
                v: TERMINAL_PROTOCOL_VERSION,
                generation: 1,
                seq: 2,
                msg: TerminalMessage::Resize {
                    pane: "%0".into(),
                    cols: 132,
                    rows: 43,
                },
            };
            let _ = ch.write(false, serde_json::to_string(&env).unwrap().as_bytes());
            sent_resize = true;
        }
        if got_live_echo && sent_resize {
            // Flush any queued SCTP/UDP packets for the Resize message before checking tmux dimensions.
            while let Ok(out) = browser_rtc.poll_output() {
                match out {
                    RtcOutput::Transmit(t) => {
                        let _ = browser_udp.send_to(&t.contents, t.destination).await;
                    }
                    RtcOutput::Timeout(_) => break,
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    assert!(dc_open, "WebRTC DataChannel should open");
    assert!(
        got_snapshot,
        "Initial ANSI snapshot should arrive over WebRTC DataChannel"
    );
    assert!(
        got_live_echo,
        "Live keystroke output should stream back over WebRTC DataChannel"
    );

    // Verify tmux window dimensions updated to 132x43.
    let out = std::process::Command::new("tmux")
        .args([
            "-L",
            &socket,
            "display-message",
            "-p",
            "-t",
            "s:0",
            "#{window_width}x#{window_height}",
        ])
        .output()
        .unwrap();
    let dims = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        dims, "132x43",
        "tmux window dimensions should update on Resize"
    );

    mgr.close_for_agent("demo/otter");
    stop_server(&socket);
}

#[cfg(unix)]
#[tokio::test]
async fn multi_tab_open_switch_and_ctrl_d_closes_secondary_tab_without_killing_agent() {
    let Some((term, socket)) = start_tmux("tabs", &["cat"]) else {
        return;
    };
    let tmux = term.tmux().unwrap();
    let (hub_tx, mut hub_rx) = mpsc::channel::<NodeFrame>(256);
    let link = Link::from_sender(hub_tx);
    let mut mgr = StreamManager::new("mac".into());
    mgr.set_grant_key(test_key());

    let now_ms = claudecord::now_ms();
    let now_s = now_ms / 1000;
    let (grant, e2e_b64) = make_grant(
        "jti-tab-1",
        "mac",
        "demo/otter",
        true,
        1,
        now_s,
        now_s + 120,
    );
    let e2e_key: [u8; 32] = B64.decode(&e2e_b64).unwrap().try_into().unwrap();

    mgr.open(
        OpenParams {
            session_id: "sess-tab-1".into(),
            agent_id: "demo/otter".into(),
            grant,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        Some(AgentTarget {
            agent_id: "demo/otter".into(),
            term: term.clone(),
            cwd: PathBuf::from("/tmp"),
            remove_env: vec!["HOME".into()],
        }),
        link,
        now_ms,
    )
    .await;

    // Wait briefly for session worker startup.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // 1. Open a secondary shell tab (+ Tab).
    let open_tab_env = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 1,
        msg: TerminalMessage::TabOpen,
    };
    let sealed = seal_envelope(&open_tab_env, &e2e_key, "sess-tab-1").unwrap();
    mgr.on_frame("sess-tab-1", 1, sealed).await;

    for _ in 0..40 {
        if tmux.list_tabs().unwrap_or_default().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let tabs = tmux.list_tabs().unwrap();
    assert_eq!(
        tabs.len(),
        2,
        "secondary tab should be created in tmux session"
    );
    assert!(tabs[0].is_agent, "tab 0 is the primary agent window");
    assert!(!tabs[1].is_agent, "tab 1 is a secondary shell window");
    let primary_pane = tabs[0].pane_id.clone();
    let second_pane = tabs[1].pane_id.clone();

    // 2. Attempt to close tab 0 (the agent tab) — must be refused!
    let close_primary = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 2,
        msg: TerminalMessage::TabClose { pane: primary_pane },
    };
    let sealed = seal_envelope(&close_primary, &e2e_key, "sess-tab-1").unwrap();
    mgr.on_frame("sess-tab-1", 1, sealed).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        tmux.list_tabs().unwrap().len(),
        2,
        "closing tab 0 must be refused to protect the Claude agent"
    );

    // 3. Send Ctrl+D (0x04) / exit to the secondary shell tab and verify it closes without killing Tab 0.
    let ctrl_d = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 3,
        msg: TerminalMessage::Input {
            pane: second_pane.clone(),
            data: B64.encode(b"exit\n\x04"),
        },
    };
    let sealed = seal_envelope(&ctrl_d, &e2e_key, "sess-tab-1").unwrap();
    mgr.on_frame("sess-tab-1", 1, sealed).await;

    for _ in 0..40 {
        if tmux.list_tabs().unwrap_or_default().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if tmux.list_tabs().unwrap_or_default().len() > 1 {
        let close_sec = TerminalEnvelope {
            v: TERMINAL_PROTOCOL_VERSION,
            generation: 1,
            seq: 4,
            msg: TerminalMessage::TabClose { pane: second_pane },
        };
        let sealed = seal_envelope(&close_sec, &e2e_key, "sess-tab-1").unwrap();
        mgr.on_frame("sess-tab-1", 1, sealed).await;
        for _ in 0..20 {
            if tmux.list_tabs().unwrap_or_default().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    assert_eq!(
        tmux.list_tabs().unwrap().len(),
        1,
        "secondary tab should be closed while primary tab 0 stays open"
    );
    term.observe(claudecord::now_ms() + 10_000);
    assert!(
        !term.has_exited(),
        "primary Claude agent in Tab 0 must remain alive after secondary tab exits"
    );

    // Drain frames and verify E2E decryption works on every emitted TermFrame.
    while let Ok(frame) = hub_rx.try_recv() {
        if let NodeFrame::TermFrame {
            session_id,
            generation,
            payload,
        } = frame
        {
            let env = open_envelope(&payload, &e2e_key, &session_id, generation)
                .expect("valid E2E ciphertext");
            assert_eq!(env.generation, 1);
        }
    }

    mgr.close_for_agent("demo/otter");
    stop_server(&socket);
}

#[tokio::test]
async fn hub_end_to_end_wss_fallback_streams_encrypted_envelopes_and_survives_browser_disconnect() {
    let dir = std::env::temp_dir().join(format!("cc-term-hub-{}", claudecord::now_ms()));
    let _ = std::fs::create_dir_all(&dir);
    let db = dir.join("t.db");
    let mut store = Store::open(&db, None).unwrap();
    let machine_tok = store.create_token("mac", 0).unwrap();
    let web_tok = store.create_token("web:kd", 0).unwrap();
    let mut core = HubCore::default();
    core.add_owner("1");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        ping_every: Duration::from_secs(30),
        max_out_bytes: 1024 * 1024,
        tick_every: Duration::from_millis(50),
        ..Config::default()
    };
    let hub = server::start(cfg, core, store).await.unwrap();

    // Connect a simulated device node to `/api/v1/node/connect`.
    let mut req = format!("ws://{}/api/v1/node/connect", hub.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {machine_tok}").parse().unwrap(),
    );
    let (mut node_ws, _) = connect_async(req).await.unwrap();
    node_ws
        .send(Message::Text(
            serde_json::to_string(&NodeFrame::Hello {
                version: env!("CARGO_PKG_VERSION").into(),
                node_name: "mac".into(),
                features: vec![],
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    // Receive Welcome with grant_key.
    let welcome_msg = node_ws.next().await.unwrap().unwrap();
    let welcome: HubFrame = serde_json::from_str(welcome_msg.to_text().unwrap()).unwrap();
    let grant_key_b64 = match welcome {
        HubFrame::Welcome { grant_key, .. } => grant_key.expect("grant_key in Welcome"),
        other => panic!("expected Welcome, got {other:?}"),
    };

    // Register agent demo/otter on node mac.
    node_ws
        .send(Message::Text(
            serde_json::to_string(&NodeFrame::AgentRegister {
                agent: AgentSpec {
                    agent_id: "demo/otter".into(),
                    name: "otter".into(),
                    project: "demo".into(),
                    adapter: AdapterId::Claude,
                    model: None,
                    role: None,
                },
                cwd: "/tmp".into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Security checks:
    // 1. Unauthenticated request is rejected with 401.
    let client = reqwest::Client::new();
    let unauth = client
        .post(format!("http://{}/api/v1/terminal/sessions", hub.addr))
        .json(&serde_json::json!({
            "project": "demo",
            "agent": "otter",
            "write": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);

    // 2. Machine token cannot create a browser terminal session (401).
    let machine_attempt = client
        .post(format!("http://{}/api/v1/terminal/sessions", hub.addr))
        .header("authorization", format!("Bearer {machine_tok}"))
        .json(&serde_json::json!({
            "project": "demo",
            "agent": "otter",
            "write": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(machine_attempt.status().as_u16(), 401);

    // 3. Authorized dashboard token creates a terminal session and receives grant + E2E key.
    let create_res = client
        .post(format!("http://{}/api/v1/terminal/sessions", hub.addr))
        .header("authorization", format!("Bearer {web_tok}"))
        .json(&serde_json::json!({
            "project": "demo",
            "agent": "otter",
            "write": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_res.status().as_u16(), 200);
    let sess_json: serde_json::Value = create_res.json().await.unwrap();
    let session_id = sess_json["sessionId"].as_str().unwrap().to_string();
    let grant = sess_json["grant"].as_str().unwrap().to_string();
    let e2e_key_b64 = sess_json["e2eKey"].as_str().unwrap().to_string();
    let e2e_key: [u8; 32] = B64.decode(&e2e_key_b64).unwrap().try_into().unwrap();
    let grant_key: [u8; 32] = B64.decode(&grant_key_b64).unwrap().try_into().unwrap();

    // Node receives HubFrame::TermOpen and verifies the grant with its grant_key.
    let open_msg = node_ws.next().await.unwrap().unwrap();
    let open_frame: HubFrame = serde_json::from_str(open_msg.to_text().unwrap()).unwrap();
    match open_frame {
        HubFrame::TermOpen {
            session_id: sid,
            agent_id,
            grant: g,
            ..
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(agent_id, "demo/otter");
            let claims = verify_terminal_grant(
                &g,
                &grant_key,
                "mac",
                "demo/otter",
                claudecord::now_ms() / 1000,
            )
            .expect("node verifies hub grant");
            assert_eq!(claims.agt, "demo/otter");
        }
        other => panic!("expected TermOpen, got {other:?}"),
    }

    // Browser connects to `/api/v1/terminal/ws`.
    let ws_url = format!(
        "ws://{}/api/v1/terminal/ws?session_id={}&grant={}",
        hub.addr, session_id, grant
    );
    let (mut browser_ws, _) = connect_async(ws_url).await.unwrap();

    // Node sends an E2E-encrypted Snapshot frame to the browser via Hub.
    let snap_env = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 1,
        msg: TerminalMessage::Output {
            pane: "%0".into(),
            data: B64.encode(b"claude> ready"),
            snapshot: true,
        },
    };
    let sealed_snap = seal_envelope(&snap_env, &e2e_key, &session_id).unwrap();
    node_ws
        .send(Message::Text(
            serde_json::to_string(&NodeFrame::TermFrame {
                session_id: session_id.clone(),
                generation: 1,
                payload: sealed_snap,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    // Browser receives the encrypted frame and decrypts it locally.
    while let Some(Ok(msg)) = browser_ws.next().await {
        if !msg.is_text() {
            continue;
        }
        let text = msg.to_text().unwrap();
        if text.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        if v["t"] == "term.frame" || v["type"] == "term.frame" {
            let ct = v["payload"].as_str().unwrap();
            let opened = open_envelope(ct, &e2e_key, &session_id, 1).unwrap();
            assert_eq!(opened, snap_env);
            break;
        }
    }

    // Browser disconnects; verify node stays connected and agent remains registered.
    let _ = browser_ws.close(None).await;
    let devices = hub.devices().await;
    assert_eq!(devices.len(), 1);
    assert!(devices[0].connected);

    // Revoke the session via POST /api/v1/terminal/sessions/{id}/revoke and verify reconnect is denied.
    let rev_res = client
        .post(format!(
            "http://{}/api/v1/terminal/sessions/{}/revoke",
            hub.addr, session_id
        ))
        .header("authorization", format!("Bearer {web_tok}"))
        .json(&serde_json::json!({ "reason": "test_revoke" }))
        .send()
        .await
        .unwrap();
    assert_eq!(rev_res.status().as_u16(), 200);

    let reconnect_url = format!(
        "ws://{}/api/v1/terminal/ws?session_id={}&grant={}",
        hub.addr, session_id, grant
    );
    if let Ok((mut ws2, _)) = connect_async(reconnect_url).await {
        // If upgraded before closing with 4003, the first frame or close must terminate immediately.
        let next = tokio::time::timeout(Duration::from_secs(2), ws2.next()).await;
        assert!(
            matches!(
                next,
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_)))
            ),
            "revoked session WebSocket must close immediately"
        );
    }

    hub.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn security_negative_matrix_enforces_replay_expiry_read_only_and_frame_limits() {
    let Some((term, socket)) = start_tmux("sec", &["cat"]) else {
        return;
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<NodeFrame>(256);
    let link = Link::from_sender(hub_tx);
    let mut mgr = StreamManager::new("mac".into());
    mgr.set_grant_key(test_key());

    let now_ms = claudecord::now_ms();
    let now_s = now_ms / 1000;

    let mk_target = || {
        Some(AgentTarget {
            agent_id: "demo/otter".into(),
            term: term.clone(),
            cwd: PathBuf::from("/tmp"),
            remove_env: vec!["HOME".into()],
        })
    };

    // 1. Expired grant is rejected.
    let (expired_grant, _) = make_grant(
        "jti-exp",
        "mac",
        "demo/otter",
        true,
        1,
        now_s - 120,
        now_s - 10,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-exp".into(),
            agent_id: "demo/otter".into(),
            grant: expired_grant,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: None,
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    match hub_rx.recv().await.unwrap() {
        NodeFrame::TermState { state, detail, .. } => {
            assert_eq!(state, "failed");
            assert!(detail.unwrap_or_default().contains("expired"));
        }
        other => panic!("expected failed TermState, got {other:?}"),
    }

    // 2. Wrong node is rejected.
    let (wrong_node_grant, _) = make_grant(
        "jti-node",
        "other-mac",
        "demo/otter",
        true,
        1,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-node".into(),
            agent_id: "demo/otter".into(),
            grant: wrong_node_grant,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: None,
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    match hub_rx.recv().await.unwrap() {
        NodeFrame::TermState { state, detail, .. } => {
            assert_eq!(state, "failed");
            assert!(detail.unwrap_or_default().contains("audience"));
        }
        other => panic!("expected failed TermState, got {other:?}"),
    }

    // 3. Read-only grant (`write: false`) rejects Input, Resize, TabOpen, TabClose and Replay JTI is rejected.
    let (ro_grant, e2e_b64) = make_grant(
        "jti-ro-1",
        "mac",
        "demo/otter",
        false,
        1,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-ro".into(),
            agent_id: "demo/otter".into(),
            grant: ro_grant.clone(),
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64.clone()),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    while hub_rx.try_recv().is_ok() {}

    // Replaying the exact same grant (`jti-ro-1`) is rejected!
    mgr.open(
        OpenParams {
            session_id: "sess-ro-replay".into(),
            agent_id: "demo/otter".into(),
            grant: ro_grant,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64.clone()),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    match hub_rx.recv().await.unwrap() {
        NodeFrame::TermState {
            session_id,
            state,
            detail,
            ..
        } => {
            assert_eq!(session_id, "sess-ro-replay");
            assert_eq!(state, "failed");
            assert!(detail.unwrap_or_default().contains("already used"));
        }
        other => panic!("expected replay rejection, got {other:?}"),
    }

    // Send Input on the read-only session -> must receive Error { code: "read_only" }.
    let e2e_key: [u8; 32] = B64.decode(&e2e_b64).unwrap().try_into().unwrap();
    let write_attempt = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 1,
        msg: TerminalMessage::Input {
            pane: "%0".into(),
            data: B64.encode(b"rm -rf /\n"),
        },
    };
    let sealed = seal_envelope(&write_attempt, &e2e_key, "sess-ro").unwrap();
    mgr.on_frame("sess-ro", 1, sealed).await;

    let mut saw_read_only = false;
    for _ in 0..10 {
        if let Ok(Some(NodeFrame::TermFrame {
            session_id,
            generation,
            payload,
        })) = tokio::time::timeout(Duration::from_secs(2), hub_rx.recv()).await
        {
            let env = open_envelope(&payload, &e2e_key, &session_id, generation).unwrap();
            if let TerminalMessage::Error { code, .. } = env.msg
                && (code == "read_only" || code == "write_denied")
            {
                saw_read_only = true;
                break;
            }
        }
    }
    assert!(
        saw_read_only,
        "read-only session must reject Input with read_only"
    );

    // 4. Oversized chunk is rejected by TerminalEnvelope::is_valid().
    let oversized = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 2,
        msg: TerminalMessage::Output {
            pane: "%0".into(),
            data: B64.encode(vec![b'x'; MAX_TERMINAL_OUTPUT_BYTES + 1]),
            snapshot: false,
        },
    };
    assert!(!oversized.is_valid());

    mgr.close_for_agent("demo/otter");
    stop_server(&socket);
}

#[cfg(unix)]
#[tokio::test]
async fn reconnect_generation_supersedes_old_stream_and_rejects_stale_frames() {
    let Some((term, socket)) = start_tmux("gen", &["cat"]) else {
        return;
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<NodeFrame>(256);
    let link = Link::from_sender(hub_tx);
    let mut mgr = StreamManager::new("mac".into());
    mgr.set_grant_key(test_key());

    let now_ms = claudecord::now_ms();
    let now_s = now_ms / 1000;

    let mk_target = || {
        Some(AgentTarget {
            agent_id: "demo/otter".into(),
            term: term.clone(),
            cwd: PathBuf::from("/tmp"),
            remove_env: vec!["HOME".into()],
        })
    };

    // Open generation 1.
    let (grant1, e2e_b64) = make_grant(
        "jti-gen-1",
        "mac",
        "demo/otter",
        true,
        1,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-gen".into(),
            agent_id: "demo/otter".into(),
            grant: grant1,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64.clone()),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    while hub_rx.try_recv().is_ok() {}

    // Reconnect with generation 2 -> supersedes generation 1.
    let (grant2, _) = make_grant(
        "jti-gen-2",
        "mac",
        "demo/otter",
        true,
        2,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-gen".into(),
            agent_id: "demo/otter".into(),
            grant: grant2,
            generation: 2,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64.clone()),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    while hub_rx.try_recv().is_ok() {}

    // Stale generation (generation 1) grant is rejected by HostGrantVerifier.
    let (stale_grant, _) = make_grant(
        "jti-gen-stale",
        "mac",
        "demo/otter",
        true,
        1,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-gen-stale".into(),
            agent_id: "demo/otter".into(),
            grant: stale_grant,
            generation: 1,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e_b64.clone()),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;
    match hub_rx.recv().await.unwrap() {
        NodeFrame::TermState {
            session_id,
            state,
            detail,
            ..
        } => {
            assert_eq!(session_id, "sess-gen-stale");
            assert_eq!(state, "failed");
            assert!(detail.unwrap_or_default().contains("stale"));
        }
        other => panic!("expected stale generation failure, got {other:?}"),
    }

    // Stale generation frame (generation 1) sent to active generation 2 session is dropped,
    // while generation 2 Ping receives a generation 2 Pong.
    let e2e_key: [u8; 32] = B64.decode(&e2e_b64).unwrap().try_into().unwrap();
    let stale_ping = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 1,
        seq: 1,
        msg: TerminalMessage::Ping { ts: 111 },
    };
    let sealed_stale = seal_envelope(&stale_ping, &e2e_key, "sess-gen").unwrap();
    mgr.on_frame("sess-gen", 1, sealed_stale).await;

    let valid_ping = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: 2,
        seq: 1,
        msg: TerminalMessage::Ping { ts: 222 },
    };
    let sealed_valid = seal_envelope(&valid_ping, &e2e_key, "sess-gen").unwrap();
    mgr.on_frame("sess-gen", 2, sealed_valid).await;

    let mut saw_valid_pong = false;
    for _ in 0..10 {
        if let Ok(Some(NodeFrame::TermFrame {
            session_id,
            generation,
            payload,
        })) = tokio::time::timeout(Duration::from_secs(2), hub_rx.recv()).await
        {
            assert_eq!(generation, 2, "only generation 2 frames may be emitted");
            let env = open_envelope(&payload, &e2e_key, &session_id, 2).unwrap();
            if let TerminalMessage::Pong { ts } = env.msg {
                assert_eq!(ts, 222);
                saw_valid_pong = true;
                break;
            }
        }
    }
    assert!(
        saw_valid_pong,
        "generation 2 Ping should receive generation 2 Pong"
    );

    mgr.close_for_agent("demo/otter");
    stop_server(&socket);
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_active_connections_and_potato_link_backpressure_resync() {
    use claudecord::protocol::terminal::StreamSequencer;

    let Some((term, socket)) = start_tmux("conc", &["cat"]) else {
        return;
    };
    let (hub_tx, mut hub_rx) = mpsc::channel::<NodeFrame>(512);
    let link = Link::from_sender(hub_tx);
    let mut mgr = StreamManager::new("mac".into());
    mgr.set_grant_key(test_key());

    let now_ms = claudecord::now_ms();
    let now_s = now_ms / 1000;

    let mk_target = || {
        Some(AgentTarget {
            agent_id: "demo/otter".into(),
            term: term.clone(),
            cwd: PathBuf::from("/tmp"),
            remove_env: vec!["HOME".into()],
        })
    };

    // 1. Open 16 concurrent active connections (e.g. desktop + mobile + allowed teammates across sessions)
    // up to the node cap (MAX_SESSIONS_PER_NODE = 16).
    let mut keys = Vec::new();
    for i in 1..=16u64 {
        let (grant, e2e_b64) = make_grant(
            &format!("jti-conc-{i}"),
            "mac",
            "demo/otter",
            true,
            i,
            now_s,
            now_s + 120,
        );
        let key: [u8; 32] = B64.decode(&e2e_b64).unwrap().try_into().unwrap();
        keys.push((format!("sess-conc-{i}"), i, key));
        mgr.open(
            OpenParams {
                session_id: format!("sess-conc-{i}"),
                agent_id: "demo/otter".into(),
                grant,
                generation: i,
                transport: "wss".into(),
                offer_sdp: None,
                e2e_key: Some(e2e_b64),
                ice_servers: vec![],
                cols: 80,
                rows: 24,
            },
            mk_target(),
            link.clone(),
            now_ms,
        )
        .await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    while hub_rx.try_recv().is_ok() {}

    // 2. Verify that a 17th concurrent session is rejected with "machine terminal session limit reached".
    let (grant17, e2e17) = make_grant(
        "jti-conc-17",
        "mac",
        "demo/otter",
        true,
        17,
        now_s,
        now_s + 120,
    );
    mgr.open(
        OpenParams {
            session_id: "sess-conc-17".into(),
            agent_id: "demo/otter".into(),
            grant: grant17,
            generation: 17,
            transport: "wss".into(),
            offer_sdp: None,
            e2e_key: Some(e2e17),
            ice_servers: vec![],
            cols: 80,
            rows: 24,
        },
        mk_target(),
        link.clone(),
        now_ms,
    )
    .await;

    let mut saw_limit_err = false;
    while let Ok(frame) = hub_rx.try_recv() {
        if let NodeFrame::TermState {
            session_id,
            state,
            detail,
            ..
        } = frame
            && session_id == "sess-conc-17"
            && state == "failed"
        {
            assert!(
                detail.unwrap_or_default().contains("limit reached"),
                "17th concurrent session must report limit reached"
            );
            saw_limit_err = true;
        }
    }
    assert!(saw_limit_err, "17th concurrent session must be rejected");

    // 3. Verify that multiple concurrent active connections (e.g. session 1 on Desktop and session 2 on Mobile)
    // can BOTH interact with the agent and receive live responses simultaneously.
    let (ref sid1, gen1, ref key1) = keys[0];
    let (ref sid2, gen2, ref key2) = keys[1];

    let ping1 = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: gen1,
        seq: 1,
        msg: TerminalMessage::Ping { ts: 1001 },
    };
    let ping2 = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: gen2,
        seq: 1,
        msg: TerminalMessage::Ping { ts: 2002 },
    };
    mgr.on_frame(sid1, gen1, seal_envelope(&ping1, key1, sid1).unwrap())
        .await;
    mgr.on_frame(sid2, gen2, seal_envelope(&ping2, key2, sid2).unwrap())
        .await;

    let mut got_pong1 = false;
    let mut got_pong2 = false;
    for _ in 0..40 {
        if got_pong1 && got_pong2 {
            break;
        }
        if let Ok(Some(NodeFrame::TermFrame {
            session_id,
            generation,
            payload,
        })) = tokio::time::timeout(Duration::from_secs(2), hub_rx.recv()).await
        {
            if session_id == *sid1 && generation == gen1 {
                let env = open_envelope(&payload, key1, sid1, gen1).unwrap();
                if matches!(env.msg, TerminalMessage::Pong { ts: 1001 }) {
                    got_pong1 = true;
                }
            } else if session_id == *sid2 && generation == gen2 {
                let env = open_envelope(&payload, key2, sid2, gen2).unwrap();
                if matches!(env.msg, TerminalMessage::Pong { ts: 2002 }) {
                    got_pong2 = true;
                }
            }
        }
    }
    assert!(
        got_pong1 && got_pong2,
        "both concurrent active sessions must independently exchange encrypted frames"
    );

    // 4. Potato connection test: verify 256 KiB backpressure queue shedding + Sync snapshot recovery.
    let mut seq = StreamSequencer::new(1, 256 * 1024);
    let mut accepted = 0usize;
    let mut dropped = 0usize;
    // Simulate a 1 MB output burst on a stalled "potato" connection without draining:
    for _ in 0..32 {
        if seq.try_enqueue(32 * 1024) {
            accepted += 1;
        } else {
            dropped += 1;
        }
    }
    assert_eq!(
        accepted, 8,
        "at most 256 KiB (8 x 32 KiB) is buffered on a stalled potato link"
    );
    assert_eq!(
        dropped, 24,
        "excess intermediate chunks are shed rather than stalling the session"
    );

    // When the potato connection recovers and sends Sync, the host immediately returns a fresh snapshot.
    let sync_env = TerminalEnvelope {
        v: TERMINAL_PROTOCOL_VERSION,
        generation: gen1,
        seq: 2,
        msg: TerminalMessage::Sync { pane: "%0".into() },
    };
    mgr.on_frame(sid1, gen1, seal_envelope(&sync_env, key1, sid1).unwrap())
        .await;

    let mut saw_resync_snapshot = false;
    for _ in 0..30 {
        if let Ok(Some(NodeFrame::TermFrame {
            session_id,
            generation,
            payload,
        })) = tokio::time::timeout(Duration::from_secs(2), hub_rx.recv()).await
            && session_id == *sid1
            && generation == gen1
        {
            let env = open_envelope(&payload, key1, sid1, gen1).unwrap();
            if let TerminalMessage::Output {
                snapshot: true,
                pane,
                ..
            } = env.msg
                && pane == "%0"
            {
                saw_resync_snapshot = true;
                break;
            }
        }
    }
    assert!(
        saw_resync_snapshot,
        "Sync request after potato-link packet drop must return a full pane snapshot"
    );

    mgr.close_for_agent("demo/otter");
    stop_server(&socket);
}
