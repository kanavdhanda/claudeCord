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
            extra_owners: vec![],
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
        features: vec![],
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
            say_id: None,
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
    // Moving it (the dashboard's Move button) to a project nobody has placed in Discord is refused, to one that is placed it goes, and from then
    // on its words appear in that project's channel and not in the first.
    let move_to = |project: &str, to: &str| {
        let (b, s, project, to) = (base.clone(), s.clone(), project.to_string(), to.to_string());
        async move {
            let r = client()
                .post(format!("{b}/api/v1/projects/{project}/agents/otter/move"))
                .header("cookie", format!("cc_session={s}"))
                .json(&json!({"to": to}))
                .send()
                .await
                .unwrap();
            (
                r.status().as_u16(),
                r.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };
    assert_eq!(
        move_to("alpha", "gamma").await.0,
        409,
        "gamma has no channel yet"
    );
    assert_eq!(move_to("alpha", "bad name!").await.0, 400);
    let (st, ch2) = post_json(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g2/channels"),
        Some(&s),
        json!({"name": "Team-Beta"}),
    )
    .await;
    assert_eq!(st, 200, "{ch2}");
    let chan2 = ch2["id"].as_str().unwrap().to_string();
    let placed = client()
        .put(format!("{base}/api/v1/projects/beta/target"))
        .header("cookie", format!("cc_session={s}"))
        .json(&json!({"bot": bot_id, "guild": "g2", "channel": chan2}))
        .send()
        .await
        .unwrap();
    assert_eq!(placed.status().as_u16(), 200);
    let (st, body) = move_to("alpha", "beta").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        move_to("alpha", "beta").await.0,
        403,
        "it is no longer in alpha"
    );
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::AgentSay {
            agent_id: "alpha/otter".into(),
            text: "now in beta".into(),
            thread: None,
            say_id: None,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let mut channel = None;
    for _ in 0..200 {
        let posts = fake.log.lock().unwrap().posts.clone();
        if let Some(p) = posts.iter().find(|p| {
            p["content"]
                .as_str()
                .is_some_and(|c| c.contains("now in beta"))
        }) {
            channel = p["channel"].as_str().map(String::from);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        channel.as_deref(),
        Some(chan2.as_str()),
        "its words follow it to the new project's channel"
    );
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

#[tokio::test]
async fn a_project_is_chosen_on_the_dashboard_by_the_owner_of_the_machine_and_nobody_else() {
    let (_gw, base, _fake) = rig_discord().await;
    let alice = sign_in(&base, "1").await;
    let bob = sign_in(&base, "2").await;
    let token = enroll(&base, &alice, "mac").await;
    // An older machine: connected, but it does not say it can be asked to start agents, so it collects the choice and starts the agent itself.
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::Hello {
            node_name: "mac".into(),
            version: "0.2.5".into(),
            features: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let c = client();
    let ask: Value = c
        .post(format!("{base}/api/device/pick"))
        .bearer_auth(&token)
        .json(&json!({"folder": "shop", "project": "eeg"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = ask["code"].as_str().expect("a code").to_string();
    let poll = |tok: String, code: String| {
        let c = client();
        let base = base.clone();
        async move {
            let r = c
                .get(format!("{base}/api/device/pick/{code}"))
                .bearer_auth(tok)
                .send()
                .await
                .unwrap();
            (
                r.status().as_u16(),
                r.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };
    // Nothing chosen yet, so the machine is told to keep waiting.
    assert_eq!(
        poll(token.clone(), code.clone()).await.1["chosen"],
        Value::Null
    );
    // The page sees the folder and the machine, for the owner only.
    let path = format!("/api/v1/pick/{code}");
    assert_eq!(get_json(&base, &path, &alice).await.1["folder"], "shop");
    // The page is told which project the folder already belongs to, to offer it first.
    assert_eq!(get_json(&base, &path, &alice).await.1["project"], "eeg");
    assert_eq!(
        get_json(&base, &path, &bob).await.0,
        404,
        "another account cannot even see it"
    );
    assert_eq!(
        post_json(&base, &path, None, json!({"project": "x"}))
            .await
            .0,
        401
    );
    assert_eq!(
        post_json(&base, &path, Some(&bob), json!({"project": "x"}))
            .await
            .0,
        404
    );
    assert_eq!(poll("not-a-token".into(), code.clone()).await.0, 401);
    // Nothing continues without a Discord bot: the choice is refused until one is saved (the stand-in Discord confirms its token).
    assert_eq!(
        post_json(&base, &path, Some(&alice), json!({"project": "shopfront"}))
            .await
            .0,
        409
    );
    assert_eq!(
        post_json(
            &base,
            "/api/v1/bots",
            Some(&alice),
            json!({"token": bot_token("444444444444444444")})
        )
        .await
        .0,
        200
    );
    // A name that is not a project name is refused; the owner's choice goes through.
    assert_eq!(
        post_json(&base, &path, Some(&alice), json!({"project": "bad name!"}))
            .await
            .0,
        400
    );
    assert_eq!(
        post_json(
            &base,
            &path,
            Some(&alice),
            json!({"project": "shopfront", "agent": "otter", "adapter": "codex", "role": "lead"})
        )
        .await
        .0,
        200
    );
    // The machine collects it exactly once, with the agent's name, program and role the page asked for.
    let got = poll(token.clone(), code.clone()).await.1;
    assert_eq!(got["chosen"], "shopfront");
    assert_eq!(got["agent"], "otter");
    assert_eq!(got["adapter"], "codex");
    assert_eq!(got["role"], "lead");
    // A choice is made once.
    assert_eq!(
        post_json(&base, &path, Some(&alice), json!({"project": "other"}))
            .await
            .0,
        409
    );
    // The page learns what became of the agent from the machine that collected it, and from nobody else.
    let result = format!("{base}/api/device/pick/{code}/result");
    let said = |tok: String, body: Value| {
        let (c, url) = (client(), result.clone());
        async move {
            c.post(url)
                .bearer_auth(tok)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(said("not-a-token".into(), json!({"ok": true})).await, 401);
    assert_eq!(
        get_json(&base, &path, &alice).await.1["result"],
        Value::Null
    );
    assert_eq!(
        said(
            token.clone(),
            json!({"ok": false, "message": "codex was not found on this machine's PATH"})
        )
        .await,
        200
    );
    let seen = get_json(&base, &path, &alice).await.1;
    assert_eq!(seen["collected"], true);
    assert_eq!(seen["result"]["ok"], false);
    assert!(
        seen["result"]["message"]
            .as_str()
            .unwrap()
            .contains("codex")
    );
    assert_eq!(poll(token, code).await.0, 404);
}

/// Places project `p` in `channel` of `guild` with `bot`, returning the status and the answer.
async fn place(
    base: &str,
    s: &str,
    p: &str,
    bot: &str,
    guild: &str,
    channel: &str,
) -> (u16, Value) {
    let r = client()
        .put(format!("{base}/api/v1/projects/{p}/target"))
        .header("cookie", format!("cc_session={s}"))
        .json(&json!({"bot": bot, "guild": guild, "channel": channel}))
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn nothing_goes_ahead_with_a_bot_that_is_not_in_a_server_or_lacks_what_it_needs() {
    let (_gw, base, fake) = rig_discord().await;
    let s = sign_in(&base, "111").await;
    let (_, bot) = post_json(
        &base,
        "/api/v1/bots",
        Some(&s),
        json!({"token": bot_token("333333333333333333")}),
    )
    .await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let guilds_path = format!("/api/v1/bots/{bot_id}/guilds");
    let all = claudecord::perms::permissions_integer().to_string();
    // The bot is in a server but can only look at it: the dashboard says what is missing, and nothing can be set up there.
    fake.log.lock().unwrap().guilds =
        vec![json!({"id": "g1", "name": "Small", "permissions": "1024"})];
    let g = get_list(&base, &guilds_path, &s).await;
    assert_eq!(g[0]["ok"], false);
    assert!(
        g[0]["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.as_str().unwrap().contains("Manage Roles")),
        "{g:?}"
    );
    let (st, why) = post_json(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g1/channels"),
        Some(&s),
        json!({"name": "alpha"}),
    )
    .await;
    assert_eq!(st, 409, "{why}");
    assert!(why["error"].as_str().unwrap().contains("missing"), "{why}");
    // Picking a project for a folder is refused too, since no server of this account is usable.
    let token = enroll(&base, &s, "mac").await;
    // (The machine is connected: a choice for one that is not is refused for that reason, whatever the server.)
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::Hello {
            node_name: "mac".into(),
            version: "0.2.5".into(),
            features: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let ask: Value = client()
        .post(format!("{base}/api/device/pick"))
        .bearer_auth(&token)
        .json(&json!({"folder": "shop"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pick = format!("/api/v1/pick/{}", ask["code"].as_str().unwrap());
    let (st, why) = post_json(&base, &pick, Some(&s), json!({"project": "shop"})).await;
    assert_eq!(st, 409, "{why}");
    // Not in any server at all (removed, or never added): the same.
    fake.log.lock().unwrap().guilds = vec![];
    assert_eq!(
        post_json(&base, &pick, Some(&s), json!({"project": "shop"}))
            .await
            .0,
        409
    );
    // With every permission it all goes through.
    fake.log.lock().unwrap().guilds = vec![json!({"id": "g1", "name": "Big", "permissions": all})];
    assert_eq!(get_list(&base, &guilds_path, &s).await[0]["ok"], true);
    let (st, ch) = post_json(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g1/channels"),
        Some(&s),
        json!({"name": "alpha"}),
    )
    .await;
    assert_eq!(st, 200, "{ch}");
    let chan = ch["id"].as_str().unwrap().to_string();
    assert_eq!(place(&base, &s, "alpha", &bot_id, "g1", &chan).await.0, 200);
    assert_eq!(
        post_json(&base, &pick, Some(&s), json!({"project": "shop"}))
            .await
            .0,
        200
    );
    // Later the bot is removed from the server. The project says why it cannot work, to the dashboard and to the machine that asks.
    fake.log.lock().unwrap().guilds = vec![];
    let projects = get_list(&base, "/api/v1/projects", &s).await;
    let alpha = projects.iter().find(|p| p["project"] == "alpha").unwrap();
    assert!(
        alpha["problem"]
            .as_str()
            .unwrap()
            .contains("not in that Discord server"),
        "{alpha}"
    );
    let on_machine: Value = client()
        .get(format!("{base}/api/device/project/alpha"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(on_machine["placed"], true);
    assert!(on_machine["problem"].is_string(), "{on_machine}");
}

#[tokio::test]
async fn a_channel_used_by_one_service_is_refused_to_another_service_with_the_same_bot() {
    let (fake, addr) = claudecord::discord::fake::start_fake().await;
    let settings = || DiscordSettings {
        api_base: format!("http://{addr}/api"),
        gateway_url: None,
    };
    let (_gw1, real) = rig_with(settings()).await;
    let (_gw2, test) = rig_with(settings()).await;
    let token = bot_token("333333333333333333");
    let (s1, s2) = (sign_in(&real, "111").await, sign_in(&test, "222").await);
    let b1 = post_json(&real, "/api/v1/bots", Some(&s1), json!({"token": token}))
        .await
        .1["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b2 = post_json(&test, "/api/v1/bots", Some(&s2), json!({"token": token}))
        .await
        .1["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, ch) = post_json(
        &real,
        &format!("/api/v1/bots/{b1}/guilds/g1/channels"),
        Some(&s1),
        json!({"name": "shared"}),
    )
    .await;
    let chan = ch["id"].as_str().unwrap().to_string();
    assert_eq!(place(&real, &s1, "alpha", &b1, "g1", &chan).await.0, 200);
    // The second service, with the same bot, cannot take that channel, whichever way it asks.
    let (st, why) = place(&test, &s2, "alpha", &b2, "g1", &chan).await;
    assert_eq!(st, 409, "{why}");
    assert!(
        why["error"]
            .as_str()
            .unwrap()
            .contains("another claudeCord service"),
        "{why}"
    );
    // The first one can still place more projects in its own channel, and the second one can use a channel of its own.
    assert_eq!(place(&real, &s1, "beta", &b1, "g1", &chan).await.0, 200);
    let (_, mine) = post_json(
        &test,
        &format!("/api/v1/bots/{b2}/guilds/g1/channels"),
        Some(&s2),
        json!({"name": "mine"}),
    )
    .await;
    assert_eq!(
        place(&test, &s2, "alpha", &b2, "g1", mine["id"].as_str().unwrap())
            .await
            .0,
        200
    );
    drop(fake);
}

#[tokio::test]
async fn an_account_keeps_its_startup_commands_on_the_hub_and_nobody_elses_are_visible() {
    let (gw, base) = rig().await;
    let (alice, bob) = (sign_in(&base, "1").await, sign_in(&base, "2").await);
    let put = |s: &str, name: &str, body: Value| {
        let (c, url, s) = (
            client(),
            format!("{base}/api/v1/commands/{name}"),
            s.to_string(),
        );
        async move {
            c.put(url)
                .header("cookie", format!("cc_session={s}"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    // Saving needs a sign-in; a name or a program that is not allowed, an empty line, and a very long one are refused.
    assert_eq!(
        client()
            .put(format!("{base}/api/v1/commands/x"))
            .json(&json!({"command": "claude"}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        put(&alice, "bad name", json!({"command": "claude"})).await,
        400
    );
    assert_eq!(put(&alice, "x", json!({"command": "   "})).await, 400);
    assert_eq!(
        put(&alice, "x", json!({"command": "claude", "program": "bash"})).await,
        400
    );
    assert_eq!(
        put(&alice, "x", json!({"command": "a".repeat(2001)})).await,
        400
    );
    assert_eq!(
        put(&alice, "x", json!({"command": "echo \u{0}"})).await,
        400
    );
    // A good one is saved, listed with its text, replaced by the same name, and unseen by another account.
    assert_eq!(
        put(
            &alice,
            "opus",
            json!({"command": "source venv/bin/activate && claude --model opus"})
        )
        .await,
        200
    );
    assert_eq!(
        put(
            &alice,
            "fast",
            json!({"command": "codex -m mini", "program": "codex"})
        )
        .await,
        200
    );
    assert_eq!(
        put(&alice, "opus", json!({"command": "claude --model opus"})).await,
        200
    );
    let list = get_list(&base, "/api/v1/commands", &alice).await;
    assert_eq!(
        list,
        vec![
            json!({"name": "fast", "command": "codex -m mini", "program": "codex"}),
            json!({"name": "opus", "command": "claude --model opus", "program": "claude"}),
        ]
    );
    assert!(get_list(&base, "/api/v1/commands", &bob).await.is_empty());
    // Removing one: once, then it is gone.
    let del = |s: &str, name: &str| {
        let (c, url, s) = (
            client(),
            format!("{base}/api/v1/commands/{name}"),
            s.to_string(),
        );
        async move {
            c.delete(url)
                .header("cookie", format!("cc_session={s}"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["removed"]
                .clone()
        }
    };
    assert_eq!(
        del(&bob, "opus").await,
        false,
        "another account cannot remove it"
    );
    assert_eq!(del(&alice, "opus").await, true);
    assert_eq!(del(&alice, "opus").await, false);
    assert_eq!(get_list(&base, "/api/v1/commands", &alice).await.len(), 1);
    gw.shutdown().await;
}

#[test]
fn a_database_from_before_commands_gets_the_column_and_keeps_its_accounts() {
    use claudecord::control::{Command, Control};
    let dir = std::env::temp_dir().join(format!("cc-oldctl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("control.db");
    // The first release's table, without the column, with an account in it.
    {
        let c = rusqlite::Connection::open(&file).unwrap();
        c.execute_batch(
            "CREATE TABLE accounts (id TEXT PRIMARY KEY, discord_id TEXT NOT NULL UNIQUE, name TEXT NOT NULL, created INTEGER NOT NULL);
             INSERT INTO accounts VALUES ('t0000000000000001', '77', 'old-timer', 1);",
        )
        .unwrap();
    }
    for round in 0..2 {
        // Opened twice: the second time the column is already there and nothing is added again, and what was saved is still there.
        let ctl = Control::open(&file).unwrap();
        assert_eq!(
            ctl.account("t0000000000000001").unwrap().unwrap().name,
            "old-timer"
        );
        assert_eq!(ctl.commands("t0000000000000001").unwrap().len(), round);
        ctl.set_command(
            "t0000000000000001",
            "a",
            Command {
                command: "claude".into(),
                program: "claude".into(),
            },
        )
        .unwrap();
        assert_eq!(ctl.commands("t0000000000000001").unwrap().len(), 1);
    }
    // At most 50 (the one saved above is the first of them).
    let ctl = Control::open(&file).unwrap();
    for i in 0..60 {
        let r = ctl.set_command(
            "t0000000000000001",
            &format!("c{i}"),
            Command {
                command: "claude".into(),
                program: "claude".into(),
            },
        );
        assert_eq!(r.is_ok(), i < 49, "{i}: {r:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pressing_start_makes_the_hub_ask_the_machine_to_start_the_agent_with_the_saved_command() {
    let (gw, base, _fake) = rig_discord().await;
    let s = sign_in(&base, "1").await;
    assert_eq!(
        post_json(
            &base,
            "/api/v1/bots",
            Some(&s),
            json!({"token": bot_token("555555555555555555")})
        )
        .await
        .0,
        200
    );
    let save = client()
        .put(format!("{base}/api/v1/commands/opus"))
        .header("cookie", format!("cc_session={s}"))
        .json(&json!({"command": "source venv/bin/activate && claude --model opus"}))
        .send()
        .await
        .unwrap();
    assert_eq!(save.status().as_u16(), 200);
    // A machine that can be asked to start agents.
    let token = enroll(&base, &s, "mac").await;
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::Hello {
            node_name: "mac".into(),
            version: "0.2.6".into(),
            features: vec!["hub-spawn".into()],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let ask = |tok: String, folder: &str| {
        let (c, url, folder) = (
            client(),
            format!("{base}/api/device/pick"),
            folder.to_string(),
        );
        async move {
            c.post(url)
                .bearer_auth(tok)
                .json(&json!({"folder": folder}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["code"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let code = ask(token.clone(), "shop").await;
    let path = format!("/api/v1/pick/{code}");
    // A saved command the account does not have is refused, and so is a name already taken once the agent exists.
    assert_eq!(
        post_json(
            &base,
            &path,
            Some(&s),
            json!({"project": "shopfront", "command": "nothere"})
        )
        .await
        .0,
        400
    );
    let (st, body) = post_json(
        &base,
        &path,
        Some(&s),
        json!({"project": "shopfront", "agent": "Fox", "role": "lead", "command": "opus"}),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    // The machine is asked, with the command's text, the code of the waiting `claudecord`, and the page's choices.
    let mut asked = None;
    for _ in 0..80 {
        if let Ok(Some(Ok(Message::Text(t)))) =
            tokio::time::timeout(Duration::from_millis(100), ws.next()).await
            && let Some(HubFrame::Spawn {
                agent,
                command,
                pick,
            }) = HubFrame::parse(t.as_str())
        {
            asked = Some((agent, command, pick));
            break;
        }
    }
    let (agent, command, pick) = asked.expect("the machine was never asked to start the agent");
    assert_eq!(
        (
            agent.name.as_str(),
            agent.project.as_str(),
            agent.role.as_deref()
        ),
        ("fox", "shopfront", Some("lead"))
    );
    assert_eq!(
        command.as_deref(),
        Some("source venv/bin/activate && claude --model opus")
    );
    assert_eq!(pick.as_deref(), Some(code.as_str()));
    // It registers; asking for the same name again is refused before anything is sent.
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::AgentRegister {
            agent,
            cwd: "/x".into(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let again = ask(token.clone(), "shop").await;
    let (st, body) = post_json(
        &base,
        &format!("/api/v1/pick/{again}"),
        Some(&s),
        json!({"project": "shopfront", "agent": "fox"}),
    )
    .await;
    assert_eq!(st, 409, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("already has an agent called fox"),
        "{body}"
    );
    // A machine that is not connected is told so instead of being waited for.
    let ghost = enroll(&base, &s, "ghost").await;
    let lost = ask(ghost, "elsewhere").await;
    let (st, body) = post_json(
        &base,
        &format!("/api/v1/pick/{lost}"),
        Some(&s),
        json!({"project": "shopfront"}),
    )
    .await;
    assert_eq!(st, 409, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not connected"),
        "{body}"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn discord_spawn_can_name_a_saved_startup_command_with_autocomplete_and_refuses_unknown_ones()
{
    let (gw, base, fake) = rig_discord().await;
    let s = sign_in(&base, "111").await;
    let (_, bot) = post_json(
        &base,
        "/api/v1/bots",
        Some(&s),
        json!({"token": bot_token("777777777777777777")}),
    )
    .await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let (_, ch) = post_json(
        &base,
        &format!("/api/v1/bots/{bot_id}/guilds/g2/channels"),
        Some(&s),
        json!({"name": "alpha"}),
    )
    .await;
    let chan = ch["id"].as_str().unwrap().to_string();
    let placed = client()
        .put(format!("{base}/api/v1/projects/alpha/target"))
        .header("cookie", format!("cc_session={s}"))
        .json(&json!({"bot": bot_id, "guild": "g2", "channel": chan}))
        .send()
        .await
        .unwrap();
    assert_eq!(placed.status().as_u16(), 200);
    let save = client()
        .put(format!("{base}/api/v1/commands/opus"))
        .header("cookie", format!("cc_session={s}"))
        .json(&json!({"command": "source venv/bin/activate && codex -m big", "program": "codex"}))
        .send()
        .await
        .unwrap();
    assert_eq!(save.status().as_u16(), 200);
    // A machine, so there is somewhere to start the agent.
    let token = enroll(&base, &s, "mac").await;
    let mut ws = connect(&base, &token).await.unwrap();
    assert!(welcomed(&mut ws).await.is_some());
    ws.send(Message::Text(
        serde_json::to_string(&NodeFrame::Hello {
            node_name: "mac".into(),
            version: "0.2.6".into(),
            features: vec!["hub-spawn".into()],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let slash = |id: &str, kind: u64, options: Value| {
        let _ = fake.events.send(
            json!({"op": 0, "s": 5, "t": "INTERACTION_CREATE", "d": {
                "id": id, "token": "tok", "type": kind, "channel_id": chan, "application_id": "app1",
                "member": {"user": {"id": "111", "username": "owner"}},
                "data": {"name": "spawn", "options": options}
            }})
            .to_string(),
        );
    };
    let answer = |id: &str| {
        let (fake, id) = (fake.clone(), id.to_string());
        async move {
            for _ in 0..200 {
                if let Some(r) = fake
                    .log
                    .lock()
                    .unwrap()
                    .responses
                    .iter()
                    .find(|r| r["interaction"] == id.as_str())
                    .cloned()
                {
                    return r;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("no answer to {id}");
        }
    };
    // Autocomplete lists the account's saved commands that match what is typed.
    slash(
        "auto1",
        4,
        json!([{"name": "name", "type": 3, "value": "owl"}, {"name": "command", "type": 3, "value": "op", "focused": true}]),
    );
    assert_eq!(
        answer("auto1").await["choices"],
        json!([{"name": "opus", "value": "opus"}])
    );
    // A name the account does not have is refused in words, and nothing is asked of the machine.
    slash(
        "bad1",
        2,
        json!([{"name": "name", "type": 3, "value": "owl"}, {"name": "command", "type": 3, "value": "nothere"}]),
    );
    assert!(
        answer("bad1").await["content"]
            .as_str()
            .unwrap()
            .contains("no saved startup command called nothere")
    );
    // The saved one: the machine is asked with its line, and the agent is read as the program the command starts.
    slash(
        "ok1",
        2,
        json!([{"name": "name", "type": 3, "value": "owl"}, {"name": "command", "type": 3, "value": "opus"}]),
    );
    let mut got = None;
    for _ in 0..80 {
        if let Ok(Some(Ok(Message::Text(t)))) =
            tokio::time::timeout(Duration::from_millis(100), ws.next()).await
            && let Some(HubFrame::Spawn {
                agent,
                command,
                pick,
            }) = HubFrame::parse(t.as_str())
        {
            got = Some((agent, command, pick));
            break;
        }
    }
    let (agent, command, pick) = got.expect("the machine was never asked");
    assert_eq!(agent.name, "owl");
    assert_eq!(agent.adapter, claudecord::protocol::AdapterId::Codex);
    assert_eq!(
        command.as_deref(),
        Some("source venv/bin/activate && codex -m big")
    );
    assert!(
        pick.is_none(),
        "a Discord spawn is not for a waiting claudecord"
    );
    gw.shutdown().await;
}

/// One machine asks for a project to be picked, and says what it got back.
async fn ask_pick(base: &str, token: &str) -> u16 {
    client()
        .post(format!("{base}/api/device/pick"))
        .bearer_auth(token)
        .json(&json!({"folder": "shop"}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn one_account_cannot_use_up_the_waiting_places_for_everyone_elses_project_picks() {
    let (_gw, base) = rig().await;
    let (alice, bob) = (sign_in(&base, "1").await, sign_in(&base, "2").await);
    let (a_tok, b_tok) = (
        enroll(&base, &alice, "mac").await,
        enroll(&base, &bob, "mac").await,
    );
    // A machine of alice's asks again and again (a loop, a bug, a hostile token) until the hub says no.
    let mut refused = false;
    for _ in 0..600 {
        if ask_pick(&base, &a_tok).await == 429 {
            refused = true;
            break;
        }
    }
    assert!(refused, "asking is not limited at all");
    assert_eq!(
        ask_pick(&base, &b_tok).await,
        200,
        "bob's machine must still get a place while alice's are full"
    );
}

/// What each account's hub costs this process in files, threads and memory. Run by hand with
///   CC_TENANTS=300 cargo test --release --test it tenants_cost -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check: prints files, threads and memory per account (CC_TENANTS accounts, default 100)"]
async fn tenants_cost() {
    let n: usize = std::env::var("CC_TENANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let (_gw, base) = rig().await;
    let before = (
        super::procs::fds(),
        super::procs::threads(),
        super::procs::rss_kb(),
    );
    let t = std::time::Instant::now();
    for i in 0..n {
        let s = sign_in(&base, &(1000 + i).to_string()).await;
        assert_eq!(get_json(&base, "/api/v1/state", &s).await.0, 200);
        if (i + 1) % (n / 5).max(1) == 0 {
            println!(
                "{:>5} accounts: {} files, {} threads, {} MB, {:?}",
                i + 1,
                super::procs::fds(),
                super::procs::threads(),
                super::procs::rss_kb() / 1024,
                t.elapsed()
            );
        }
    }
    let after = (
        super::procs::fds(),
        super::procs::threads(),
        super::procs::rss_kb(),
    );
    println!(
        "per account: {:.1} files, {:.2} threads, {:.0} KB",
        (after.0 - before.0) as f64 / n as f64,
        (after.1 - before.1) as f64 / n as f64,
        (after.2 - before.2) as f64 / n as f64
    );
}

/// Asks the hub to exchange a refresh token (as the bearer): the status and the answer.
async fn refresh(base: &str, token: &str) -> (u16, Value) {
    let r = client()
        .post(format!("{base}/api/device/refresh"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_machine_trades_its_refresh_token_for_short_lived_access_tokens() {
    let (gw, base) = rig().await;
    let s = sign_in(&base, "111").await;
    let login = enroll(&base, &s, "mac").await;
    let (status, first) = refresh(&base, &login).await;
    assert_eq!(status, 200, "{first}");
    let access = first["access_token"].as_str().unwrap();
    let next = first["refresh_token"].as_str().unwrap();
    assert!(access.starts_with("eyJ") && next.starts_with("ccn1.") && next != login);
    assert_eq!(first["expires_in"], 900);
    // The access token opens the door, and so does the new refresh token.
    let mut ws = connect(&base, access).await.unwrap();
    assert_eq!(welcomed(&mut ws).await.as_deref(), Some("mac"));
    // The replaced one still works for a minute, so an answer lost on the way does not lock the machine out.
    assert_eq!(refresh(&base, &login).await.0, 200);
    assert_eq!(refresh(&base, next).await.0, 200);
    // A forged or made-up token never does.
    assert_eq!(refresh(&base, "ccn1.nothing").await.0, 401);
    assert_eq!(refresh(&base, &format!("{access}x")).await.0, 401);
    assert!(connect(&base, &format!("{access}x")).await.is_err());
    // Revoking the machine ends its access token at once, not when it runs out.
    post_json(
        &base,
        "/api/v1/machines/revoke",
        Some(&s),
        json!({"node": "mac"}),
    )
    .await;
    assert!(connect(&base, access).await.is_err());
    assert_eq!(refresh(&base, next).await.0, 401);
    gw.shutdown().await;
}

#[test]
fn a_replaced_refresh_token_stops_working_after_its_grace_time() {
    let c = Control::open_memory().unwrap();
    let a = c.sign_in_discord("1", "ann", 0).unwrap();
    let (device, user) = c.device_start("mac", 0).unwrap();
    assert!(c.device_approve(&user, &a.id, "mac", 1).unwrap());
    let claudecord::control::Poll::Approved { token, .. } = c.device_poll(&device, 2).unwrap()
    else {
        panic!("not approved")
    };
    let (_, _, fresh) = c.rotate_machine_token(&token, 1_000).unwrap().unwrap();
    assert!(
        c.rotate_machine_token(&token, 1_000 + 59_000)
            .unwrap()
            .is_some(),
        "inside the grace time"
    );
    assert!(
        c.rotate_machine_token(&token, 1_000 + claudecord::control::REFRESH_GRACE_MS + 1)
            .unwrap()
            .is_none(),
        "after it"
    );
    assert!(
        c.rotate_machine_token(&fresh, 1_000 + 120_000)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_machine_side_asks_for_an_access_token_once_and_keeps_it_until_it_is_nearly_out() {
    let (gw, base) = rig().await;
    let s = sign_in(&base, "111").await;
    let login = enroll(&base, &s, "mac").await;
    let dir = std::env::temp_dir().join(format!("cc-auth-{}", std::process::id()));
    let cfg = claudecord::device::config::Config {
        hub_url: base.clone(),
        token: login.clone(),
        node_name: "mac".into(),
    };
    cfg.save(&dir).unwrap();
    let auth = claudecord::device::link::Auth::new(&base, login.clone(), Some(dir.clone()));
    let first = auth.bearer().await.unwrap();
    assert!(
        first.starts_with("eyJ"),
        "an access token, not the login: {first}"
    );
    assert_eq!(
        auth.bearer().await.unwrap(),
        first,
        "kept, not asked for again"
    );
    // The new refresh token was saved in the config, in place of the one the person logged in with.
    let saved = claudecord::device::config::Config::load(&dir).unwrap();
    assert!(saved.token.starts_with("ccn1.") && saved.token != login);
    assert!(connect(&base, &first).await.is_ok());
    std::fs::remove_dir_all(&dir).ok();
    gw.shutdown().await;
}

#[tokio::test]
async fn a_machines_http_requests_accept_an_access_token_and_refuse_a_forged_or_revoked_one() {
    let (gw, base) = rig().await;
    let s = sign_in(&base, "111").await;
    let login = enroll(&base, &s, "mac").await;
    let (_, tokens) = refresh(&base, &login).await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let ask = |t: String| {
        let url = format!("{base}/api/device/project/demo");
        async move {
            client()
                .get(url)
                .bearer_auth(t)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(ask(access.clone()).await, 200, "an access token opens it");
    assert_eq!(
        ask(format!("{access}x")).await,
        401,
        "a changed one does not"
    );
    post_json(
        &base,
        "/api/v1/machines/revoke",
        Some(&s),
        json!({"node": "mac"}),
    )
    .await;
    assert_eq!(
        ask(access).await,
        401,
        "and a revoked machine's stops at once"
    );
    gw.shutdown().await;
}
