//! The hosted front door against a stand-in Discord sign-in: accounts are made on first sign-in, a machine with no token is approved by
//! a person through a short code, the token it gets connects to THAT account's hub and no other, and one account never sees another's
//! machines. Each test fails if the isolation or the approval check it is about is removed.

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use claudecord::control::registry::DiscordSettings;
use claudecord::control::{Control, seal::LocalKeys};
use claudecord::device::enroll;
use claudecord::protocol::{HubFrame, NodeFrame};
use claudecord::server::{
    Config, Oauth,
    gateway::{GatewayConfig, GatewayHandle, start_gateway},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::{
    connect_async, tungstenite::Message, tungstenite::client::IntoClientRequest,
};

/// The stand-in Discord. The code `code-for-<id>` signs in account `<id>`.
async fn fake_discord() -> String {
    type Seen = Arc<Mutex<String>>;
    async fn token(State(_): State<Seen>, body: String) -> Json<Value> {
        let code = body
            .split('&')
            .find_map(|p| p.strip_prefix("code="))
            .unwrap_or("");
        Json(json!({"access_token": format!("at-{code}")}))
    }
    async fn me(
        State(_): State<Seen>,
        headers: HeaderMap,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let id = auth
            .strip_prefix("Bearer at-code-for-")
            .ok_or(axum::http::StatusCode::UNAUTHORIZED)?
            .to_string();
        Ok(Json(json!({"id": id, "username": format!("user{id}")})))
    }
    let app = Router::new()
        .route("/oauth2/token", post(token))
        .route("/users/@me", get(me))
        .with_state(Arc::new(Mutex::new(String::new())));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

async fn rig() -> (GatewayHandle, String) {
    rig_with(Default::default()).await
}

async fn rig_with(discord_api: DiscordSettings) -> (GatewayHandle, String) {
    let discord = fake_discord().await;
    let dir = std::env::temp_dir().join(format!(
        "cc-gw-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open(&dir.join("control.db")).unwrap());
    let gw = start_gateway(
        GatewayConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "http://127.0.0.1".into(),
            oauth: Some(Oauth {
                client_id: "app1".into(),
                client_secret: "sekret".into(),
                redirect_uri: "http://127.0.0.1/auth/callback".into(),
                authorize_url: format!("{discord}/authorize"),
                api_base: discord,
            }),
            hub: Config::default(),
            discord: discord_api,
            dev: false,
            bucket: None,
        },
        dir,
        control,
        Arc::new(LocalKeys::random()),
    )
    .await
    .unwrap();
    let base = format!("http://{}", gw.addr);
    (gw, base)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn cookie_of(r: &reqwest::Response, name: &str) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            c.strip_prefix(&format!("{name}="))
                .map(|v| v.split(';').next().unwrap().to_string())
        })
}

/// Signs a Discord person in and returns their session cookie value.
async fn sign_in(base: &str, id: &str) -> String {
    let c = client();
    let start = c.get(format!("{base}/auth/login")).send().await.unwrap();
    let state = cookie_of(&start, "cc_state").unwrap();
    let back = c
        .get(format!(
            "{base}/auth/callback?code=code-for-{id}&state={state}"
        ))
        .header("cookie", format!("cc_state={state}"))
        .send()
        .await
        .unwrap();
    assert_eq!(back.status(), 303);
    cookie_of(&back, "cc_session").expect("a session cookie")
}

async fn get_json(base: &str, path: &str, session: &str) -> (u16, Value) {
    let r = client()
        .get(format!("{base}{path}"))
        .header("cookie", format!("cc_session={session}"))
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

async fn post_json(base: &str, path: &str, session: Option<&str>, body: Value) -> (u16, Value) {
    let mut r = client().post(format!("{base}{path}")).json(&body);
    if let Some(s) = session {
        r = r.header("cookie", format!("cc_session={s}"));
    }
    let r = r.send().await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

/// A machine asks for a code, a person approves it as `node`, and the machine collects its token.
async fn enroll(base: &str, session: &str, node: &str) -> String {
    let (s, code) = post_json(base, "/api/device/code", None, json!({"node": node})).await;
    assert_eq!(s, 200);
    let device = code["device_code"].as_str().unwrap().to_string();
    let user = code["user_code"].as_str().unwrap().to_string();
    // Nobody has approved yet.
    let (s, _) = post_json(
        base,
        "/api/device/token",
        None,
        json!({"device_code": device}),
    )
    .await;
    assert_eq!(s, 202);
    let (s, _) = post_json(
        base,
        "/api/device/approve",
        Some(session),
        json!({"code": user, "node": node}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, t) = post_json(
        base,
        "/api/device/token",
        None,
        json!({"device_code": device}),
    )
    .await;
    assert_eq!(s, 200);
    t["token"].as_str().unwrap().to_string()
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(base: &str, token: &str) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!(
        "ws://{}/api/v1/node/connect",
        base.trim_start_matches("http://")
    )
    .into_client_request()
    .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(ws, _)| ws)
}

async fn welcomed(ws: &mut Ws) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Some(HubFrame::Welcome { node_id }) = HubFrame::parse(t.as_str()) {
                    return Some(node_id);
                }
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn signing_in_makes_an_account_and_a_session() {
    let (gw, base) = rig().await;
    assert_eq!(get_json(&base, "/api/v1/me", "nope").await.0, 401);
    let s = sign_in(&base, "111").await;
    let (st, me) = get_json(&base, "/api/v1/me", &s).await;
    assert_eq!(st, 200);
    assert_eq!(me["name"], "user111");
    // The same person signing in again is the same account.
    let s2 = sign_in(&base, "111").await;
    assert_eq!(get_json(&base, "/api/v1/me", &s2).await.1["id"], me["id"]);
    // Signing out ends the session on the server.
    client()
        .post(format!("{base}/auth/logout"))
        .header("cookie", format!("cc_session={s}"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_json(&base, "/api/v1/me", &s).await.0, 401);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_machine_is_approved_by_code_and_connects_to_its_owners_hub() {
    let (gw, base) = rig().await;
    let s = sign_in(&base, "111").await;
    let token = enroll(&base, &s, "mac").await;
    let mut ws = connect(&base, &token).await.unwrap();
    assert_eq!(welcomed(&mut ws).await.as_deref(), Some("mac"));
    let hello = NodeFrame::Hello {
        node_name: "mac".into(),
        version: "t".into(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    // It shows in its owner's dashboard state, as connected.
    let mut seen = false;
    for _ in 0..80 {
        let (_, st) = get_json(&base, "/api/v1/state", &s).await;
        if st["devices"].as_array().is_some_and(|d| {
            d.iter()
                .any(|x| x["node"] == "mac" && x["connected"] == true)
        }) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(seen, "the machine never showed as connected");
    let (_, list) = get_json(&base, "/api/v1/machines", &s).await;
    assert_eq!(list[0]["node"], "mac");
    gw.shutdown().await;
}

#[tokio::test]
async fn one_account_never_sees_or_reaches_another() {
    let (gw, base) = rig().await;
    let a = sign_in(&base, "111").await;
    let b = sign_in(&base, "222").await;
    let token_a = enroll(&base, &a, "alpha-box").await;
    let mut ws = connect(&base, &token_a).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    // Account B's dashboard knows nothing of A's machine, and lists none of A's machines.
    let (_, st) = get_json(&base, "/api/v1/state", &b).await;
    assert!(st["devices"].as_array().unwrap().is_empty(), "{st}");
    assert!(
        get_json(&base, "/api/v1/machines", &b)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    // B cannot revoke A's machine by name.
    let (_, r) = post_json(
        &base,
        "/api/v1/machines/revoke",
        Some(&b),
        json!({"node": "alpha-box"}),
    )
    .await;
    assert_eq!(r["revoked"], 0);
    assert_eq!(
        get_json(&base, "/api/v1/machines", &a)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn approving_needs_a_signed_in_person_and_a_code_works_once() {
    let (gw, base) = rig().await;
    let (_, code) = post_json(&base, "/api/device/code", None, json!({"node": "mac"})).await;
    let user = code["user_code"].as_str().unwrap().to_string();
    // No session: refused, and the machine is still waiting.
    let (s, _) = post_json(
        &base,
        "/api/device/approve",
        None,
        json!({"code": user, "node": "mac"}),
    )
    .await;
    assert_eq!(s, 401);
    let device = code["device_code"].as_str().unwrap();
    assert_eq!(
        post_json(
            &base,
            "/api/device/token",
            None,
            json!({"device_code": device})
        )
        .await
        .0,
        202
    );
    let me = sign_in(&base, "111").await;
    // The code is accepted typed in lower case without the dash, as people do.
    let typed = user.to_lowercase().replace('-', "");
    assert_eq!(
        post_json(
            &base,
            "/api/device/approve",
            Some(&me),
            json!({"code": typed, "node": "mac"})
        )
        .await
        .0,
        200
    );
    // A second approval of the same code (by anyone) finds it used.
    let other = sign_in(&base, "222").await;
    assert_eq!(
        post_json(
            &base,
            "/api/device/approve",
            Some(&other),
            json!({"code": user, "node": "evil"})
        )
        .await
        .0,
        404
    );
    // The token is handed over once; asking again gets nothing.
    assert_eq!(
        post_json(
            &base,
            "/api/device/token",
            None,
            json!({"device_code": device})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        post_json(
            &base,
            "/api/device/token",
            None,
            json!({"device_code": device})
        )
        .await
        .0,
        410
    );
    // A bad machine name is refused.
    let (_, c2) = post_json(&base, "/api/device/code", None, json!({})).await;
    let u2 = c2["user_code"].as_str().unwrap();
    assert_eq!(
        post_json(
            &base,
            "/api/device/approve",
            Some(&me),
            json!({"code": u2, "node": "../x"})
        )
        .await
        .0,
        400
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn guessing_codes_gets_an_address_blocked() {
    let (gw, base) = rig().await;
    let me = sign_in(&base, "111").await;
    let mut last = 0;
    for _ in 0..14 {
        last = get_json(&base, "/api/device/lookup?code=AAAA-AAAA", &me)
            .await
            .0;
    }
    assert_eq!(last, 429);
    gw.shutdown().await;
}

#[tokio::test]
async fn a_revoked_machine_cannot_connect_and_a_made_up_token_never_could() {
    let (gw, base) = rig().await;
    let s = sign_in(&base, "111").await;
    let token = enroll(&base, &s, "mac").await;
    assert!(connect(&base, "not-a-token").await.is_err());
    let (_, r) = post_json(
        &base,
        "/api/v1/machines/revoke",
        Some(&s),
        json!({"node": "mac"}),
    )
    .await;
    assert_eq!(r["revoked"], 1);
    assert!(connect(&base, &token).await.is_err());
    gw.shutdown().await;
}

#[tokio::test]
async fn a_next_page_outside_this_site_is_ignored_after_sign_in() {
    let (gw, base) = rig().await;
    let c = client();
    let start = c
        .get(format!("{base}/auth/login?next=//evil.example/x"))
        .send()
        .await
        .unwrap();
    assert!(cookie_of(&start, "cc_next").is_none());
    let start = c
        .get(format!(
            "{base}/auth/login?next=/activate%3Fcode%3DABCD-EFGH"
        ))
        .send()
        .await
        .unwrap();
    let state = cookie_of(&start, "cc_state").unwrap();
    let next = cookie_of(&start, "cc_next").unwrap();
    let back = c
        .get(format!(
            "{base}/auth/callback?code=code-for-5&state={state}"
        ))
        .header("cookie", format!("cc_state={state}; cc_next={next}"))
        .send()
        .await
        .unwrap();
    assert_eq!(back.headers()["location"], "/activate?code=ABCD-EFGH");
    gw.shutdown().await;
}

#[tokio::test]
async fn a_new_machine_enrolls_through_the_browser_flow_and_ends_up_with_a_working_config() {
    let (gw, base) = rig().await;
    let me = sign_in(&base, "111").await;
    let b2 = base.clone();
    // The "browser": when the machine shows its code, the signed-in person approves it.
    let cfg = enroll::enroll(&base, "laptop", move |code, link| {
        assert!(link.contains("/activate?code="), "{link}");
        let (b, me, code) = (b2.clone(), me.clone(), code.to_string());
        tokio::spawn(async move {
            let (s, _) = post_json(
                &b,
                "/api/device/approve",
                Some(&me),
                json!({"code": code, "node": "laptop"}),
            )
            .await;
            assert_eq!(s, 200);
        });
    })
    .await
    .unwrap();
    assert_eq!(cfg.node_name, "laptop");
    assert!(cfg.hub_url.starts_with("ws://"), "{}", cfg.hub_url);
    // What it saved is enough to connect.
    let mut ws = connect(&base, &cfg.token).await.unwrap();
    assert_eq!(welcomed(&mut ws).await.as_deref(), Some("laptop"));
    gw.shutdown().await;
}

#[test]
fn hub_addresses_are_understood_in_any_form() {
    assert_eq!(
        enroll::http_base("wss://h.example.com/"),
        "https://h.example.com"
    );
    assert_eq!(enroll::http_base("h.example.com"), "https://h.example.com");
    assert_eq!(
        enroll::ws_base("https://h.example.com"),
        "wss://h.example.com"
    );
    assert_eq!(
        enroll::ws_base("http://127.0.0.1:8787"),
        "ws://127.0.0.1:8787"
    );
}

// ----- bots and workspaces, against the stand-in Discord -----

/// A bot token shaped like a real one for the given application id.
fn bot_token(id: &str) -> String {
    use base64::Engine;
    format!(
        "{}.AAAAAA.secretsecretsecretsecret",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id)
    )
}

async fn rig_discord() -> (GatewayHandle, String, claudecord::discord::fake::Fake) {
    let (fake, addr) = claudecord::discord::fake::start_fake().await;
    let (gw, base) = rig_with(DiscordSettings {
        api_base: format!("http://{addr}/api"),
        gateway_url: None,
    })
    .await;
    (gw, base, fake)
}

async fn get_list(base: &str, path: &str, s: &str) -> Vec<Value> {
    get_json(base, path, s)
        .await
        .1
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_bot_token_is_checked_with_discord_sealed_and_never_shown_again() {
    let (gw, base, _fake) = rig_discord().await;
    let s = sign_in(&base, "111").await;
    // Not shaped like a token, and shaped but refused by Discord.
    assert_eq!(
        post_json(&base, "/api/v1/bots", Some(&s), json!({"token": "hello"}))
            .await
            .0,
        400
    );
    let bad = format!("bad{}", bot_token("222222222222222222"));
    assert_eq!(
        post_json(&base, "/api/v1/bots", Some(&s), json!({"token": bad}))
            .await
            .0,
        400
    );
    // A good one is saved.
    let token = bot_token("333333333333333333");
    let (st, bot) = post_json(&base, "/api/v1/bots", Some(&s), json!({"token": token})).await;
    assert_eq!(st, 200, "{bot}");
    assert_eq!(bot["app_id"], "333333333333333333");
    assert!(
        bot["invite_url"]
            .as_str()
            .unwrap()
            .contains("333333333333333333")
    );
    // Listings never carry it, and the stored text is not the token.
    let list = get_json(&base, "/api/v1/bots", &s).await.1;
    assert!(!list.to_string().contains("secretsecret"), "{list}");
    let me = get_json(&base, "/api/v1/me", &s).await.1;
    let stored = gw
        .control
        .sealed_bot(me["id"].as_str().unwrap(), bot["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        !stored.contains("secretsecret") && stored.starts_with("v1."),
        "{stored}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn one_accounts_bot_is_invisible_to_another() {
    let (gw, base, _fake) = rig_discord().await;
    let a = sign_in(&base, "111").await;
    let b = sign_in(&base, "222").await;
    let (_, bot) = post_json(
        &base,
        "/api/v1/bots",
        Some(&a),
        json!({"token": bot_token("333333333333333333")}),
    )
    .await;
    let id = bot["id"].as_str().unwrap();
    assert!(get_list(&base, "/api/v1/bots", &b).await.is_empty());
    assert_eq!(
        get_json(&base, &format!("/api/v1/bots/{id}/guilds"), &b)
            .await
            .0,
        404
    );
    let (st, _) = post_json(&base, "/api/v1/projects/alpha/target", None, json!({})).await;
    assert_ne!(st, 200);
    // B cannot place a project with A's bot either.
    let (st, _) = {
        let r = client()
            .put(format!("{base}/api/v1/projects/alpha/target"))
            .header("cookie", format!("cc_session={b}"))
            .json(&json!({"bot": id, "guild": "g1", "channel": "1"}))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), ())
    };
    assert_eq!(st, 404);
    // B cannot remove it.
    let r = client()
        .delete(format!("{base}/api/v1/bots/{id}"))
        .header("cookie", format!("cc_session={b}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["removed"], false);
    assert_eq!(get_list(&base, "/api/v1/bots", &a).await.len(), 1);
    gw.shutdown().await;
}

#[tokio::test]
async fn an_agents_words_reach_the_channel_chosen_in_the_chosen_server() {
    let (gw, base, fake) = rig_discord().await;
    let s = sign_in(&base, "111").await;
    let (_, bot) = post_json(
        &base,
        "/api/v1/bots",
        Some(&s),
        json!({"token": bot_token("333333333333333333")}),
    )
    .await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    // The wizard: servers, then a new channel in the second one.
    let guilds = get_list(&base, &format!("/api/v1/bots/{bot_id}/guilds"), &s).await;
    assert_eq!(guilds.len(), 2);
    let (st, ch) = post_json(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g2/channels"),
        Some(&s),
        json!({"name": "Team-Alpha"}),
    )
    .await;
    assert_eq!(st, 200, "{ch}");
    assert_eq!(ch["name"], "team-alpha");
    let chan = ch["id"].as_str().unwrap().to_string();
    let chans = get_list(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g2/channels"),
        &s,
    )
    .await;
    assert!(chans.iter().any(|c| c["id"] == chan.as_str()));
    // A channel that is not in that server is refused.
    let put = |body: Value| {
        let (b, s) = (base.clone(), s.clone());
        async move {
            client()
                .put(format!("{b}/api/v1/projects/alpha/target"))
                .header("cookie", format!("cc_session={s}"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(
        put(json!({"bot": bot_id, "guild": "g2", "channel": "999999"})).await,
        404
    );
    assert_eq!(
        put(json!({"bot": bot_id, "guild": "g2", "channel": chan})).await,
        200
    );
    // Unplaced projects are listed as such, placed ones show where.
    let projects = get_list(&base, "/api/v1/projects", &s).await;
    assert!(
        projects
            .iter()
            .any(|p| p["project"] == "alpha" && p["placed"] == true && p["guild"] == "g2")
    );
    // A machine joins and an agent in that project speaks.
    let token = enroll(&base, &s, "mac").await;
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    let spec = claudecord::protocol::AgentSpec {
        agent_id: "alpha/otter".into(),
        name: "otter".into(),
        project: "alpha".into(),
        adapter: claudecord::protocol::AdapterId::Claude,
        model: None,
        role: None,
    };
    for f in [
        NodeFrame::AgentRegister {
            agent: spec,
            cwd: "/x".into(),
        },
        NodeFrame::AgentSay {
            agent_id: "alpha/otter".into(),
            text: "hello from otter".into(),
            thread: None,
        },
    ] {
        ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
            .await
            .unwrap();
    }
    let mut got = None;
    for _ in 0..200 {
        let posts = fake.log.lock().unwrap().posts.clone();
        if let Some(p) = posts.iter().find(|p| {
            p["content"]
                .as_str()
                .is_some_and(|c| c.contains("hello from otter"))
        }) {
            got = Some(p.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let post = got.expect("the agent's words never reached Discord");
    assert_eq!(post["username"], "otter");
    // It was posted in the chosen channel, not in some other one.
    assert_eq!(post["channel"], chan.as_str(), "{post}");
    gw.shutdown().await;
}

#[tokio::test]
async fn the_dashboard_app_is_served_and_a_mistyped_api_address_is_not_a_page() {
    let (gw, base) = rig().await;
    let c = client();
    let home = c.get(format!("{base}/setup")).send().await.unwrap();
    assert_eq!(home.status(), 200);
    let csp = home.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.contains("script-src 'self'") && !csp.contains("unsafe"),
        "{csp}"
    );
    assert!(home.text().await.unwrap().contains("<div id=\"root\">"));
    assert_eq!(
        c.get(format!("{base}/app.js"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(format!("{base}/api/v1/nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn insights_show_what_agents_did_and_only_to_their_own_account() {
    let (gw, base) = rig().await;
    let a = sign_in(&base, "111").await;
    let b = sign_in(&base, "222").await;
    let token = enroll(&base, &a, "mac").await;
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    let spec = claudecord::protocol::AgentSpec {
        agent_id: "p/otter".into(),
        name: "otter".into(),
        project: "p".into(),
        adapter: claudecord::protocol::AdapterId::Claude,
        model: None,
        role: None,
    };
    for f in [
        NodeFrame::AgentRegister {
            agent: spec,
            cwd: "/x".into(),
        },
        NodeFrame::AgentStatus {
            agent_id: "p/otter".into(),
            status: claudecord::protocol::AgentStatus::Thinking,
            detail: None,
        },
        NodeFrame::AgentAsk {
            agent_id: "p/otter".into(),
            ask_id: "a1".into(),
            question: "which one?".into(),
            options: None,
            thread: None,
        },
    ] {
        ws.send(Message::Text(serde_json::to_string(&f).unwrap().into()))
            .await
            .unwrap();
    }
    let mut mine = Value::Null;
    for _ in 0..200 {
        mine = get_json(&base, "/api/v1/insights?range=1h", &a).await.1;
        if mine["summary"]["asks"]["opened"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(mine["summary"]["asks"]["opened"], 1, "{mine}");
    let lane = &mine["summary"]["lanes"][0];
    assert_eq!(lane["agent"], "otter");
    let states: Vec<&str> = lane["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["idle", "thinking", "waiting_input"]);
    // Another account sees none of it, and nobody signed out sees anything.
    let theirs = get_json(&base, "/api/v1/insights?range=1h", &b).await.1;
    assert!(
        theirs["summary"]["lanes"].as_array().unwrap().is_empty(),
        "{theirs}"
    );
    assert_eq!(get_json(&base, "/api/v1/insights", "nope").await.0, 401);
    gw.shutdown().await;
}

/// The control database itself refuses to hand one account's bot, machines or places to another, whatever the layer above checks.
#[test]
fn the_control_database_scopes_every_lookup_to_its_account() {
    let c = Control::open_memory().unwrap();
    let a = c.sign_in_discord("1", "ann", 0).unwrap();
    let b = c.sign_in_discord("2", "bob", 0).unwrap();
    let bot = c.save_bot(&a.id, "333", "robot", "v1.k1.x.y", 0).unwrap();
    assert!(c.sealed_bot(&a.id, &bot).unwrap().is_some());
    assert!(c.sealed_bot(&b.id, &bot).unwrap().is_none());
    assert!(!c.delete_bot(&b.id, &bot).unwrap());
    assert!(c.bots(&b.id).unwrap().is_empty());
    c.set_target(
        &a.id,
        &claudecord::control::Placement {
            project: "p".into(),
            bot: bot.clone(),
            guild: "g".into(),
            channel: "1".into(),
            guild_name: "G".into(),
            channel_name: "c".into(),
        },
    )
    .unwrap();
    assert!(c.target(&b.id, "p").unwrap().is_none());
    assert!(c.targets(&b.id).unwrap().is_empty());
    let (device, user) = c.device_start("mac", 0).unwrap();
    assert!(c.device_approve(&user, &a.id, "mac", 1).unwrap());
    c.device_poll(&device, 2).unwrap();
    assert_eq!(c.revoke_machine(&b.id, "mac").unwrap(), 0);
    assert_eq!(c.machines(&a.id).unwrap().len(), 1);
    assert!(c.machines(&b.id).unwrap().is_empty());
}
