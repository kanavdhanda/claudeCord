//! "Sign in with Discord" for the dashboard, against a stand-in Discord OAuth server and a real hub: the redirect, the state
//! check, who may sign in, who sees which project, signing out, and that tokens still work and no-credentials is not a failure.

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use claudecord::hub::{HubCore, Human, Role};
use claudecord::protocol::{AdapterId, AgentSpec, NodeFrame};
use claudecord::server::{self, Config, Oauth};
use claudecord::store::Store;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// The stand-in Discord. The code `code-for-<id>` signs in account `<id>`.
async fn fake_discord() -> String {
    type Seen = Arc<Mutex<String>>;
    async fn token(State(seen): State<Seen>, body: String) -> Json<Value> {
        assert!(
            body.contains("client_secret=sekret") && body.contains("grant_type=authorization_code")
        );
        let code = body
            .split('&')
            .find_map(|p| p.strip_prefix("code="))
            .unwrap_or("");
        *seen.lock().unwrap() = code.trim_start_matches("code-for-").to_string();
        Json(json!({"access_token": format!("at-{code}")}))
    }
    async fn me(
        State(seen): State<Seen>,
        headers: HeaderMap,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !auth.starts_with("Bearer at-code-for-") {
            return Err(axum::http::StatusCode::UNAUTHORIZED);
        }
        let id = auth.trim_start_matches("Bearer at-code-for-").to_string();
        let _ = seen;
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

fn human(id: &str) -> Human {
    Human {
        id: id.into(),
        name: format!("user{id}"),
    }
}

static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A hub with owner 100, project `alpha` (operator 200) and project `beta`, each with one agent.
async fn rig(oauth: bool) -> (server::Hub, String) {
    let discord = fake_discord().await;
    let mut core = HubCore::default();
    core.add_owner("100");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        oauth: oauth.then(|| Oauth {
            client_id: "app1".into(),
            client_secret: "sekret".into(),
            redirect_uri: "http://127.0.0.1/auth/callback".into(),
            authorize_url: format!("{discord}/authorize"),
            api_base: discord,
        }),
        ..Config::default()
    };
    let dir = std::env::temp_dir().join(format!(
        "cc-login-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hub = server::start(cfg, core, Store::open(&dir.join("hub.db"), None).unwrap())
        .await
        .unwrap();
    hub.call(|c, now| {
        let mut fx = vec![];
        for p in ["alpha", "beta"] {
            fx.extend(c.on_node_frame(
                "mac",
                NodeFrame::AgentRegister {
                    agent: AgentSpec {
                        agent_id: format!("{p}/otter"),
                        name: "otter".into(),
                        project: p.into(),
                        adapter: AdapterId::Claude,
                        model: None,
                        role: None,
                    },
                    cwd: "/x".into(),
                },
                now,
            ));
        }
        c.set_role(&human("100"), "alpha", &human("200"), Some(Role::Operator))
            .unwrap();
        ((), fx)
    })
    .await;
    let base = format!("http://{}", hub.addr);
    (hub, base)
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

/// Runs the whole flow for one Discord account and returns the session cookie, or the status that refused it.
async fn sign_in(base: &str, id: &str) -> Result<String, u16> {
    let c = client();
    let start = c.get(format!("{base}/auth/login")).send().await.unwrap();
    assert_eq!(start.status(), 303);
    let state = cookie_of(&start, "cc_state").expect("a state cookie");
    let back = c
        .get(format!(
            "{base}/auth/callback?code=code-for-{id}&state={state}"
        ))
        .header("cookie", format!("cc_state={state}"))
        .send()
        .await
        .unwrap();
    if back.status() != 303 {
        return Err(back.status().as_u16());
    }
    Ok(cookie_of(&back, "cc_session").expect("a session cookie"))
}

async fn get_json(base: &str, path: &str, session: &str) -> (u16, Value) {
    let r = client()
        .get(format!("{base}{path}"))
        .header("cookie", format!("cc_session={session}"))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn login_sends_the_browser_to_discord_with_a_state_it_remembers() {
    let (hub, base) = rig(true).await;
    let r = client()
        .get(format!("{base}/auth/login"))
        .send()
        .await
        .unwrap();
    let to = r.headers()["location"].to_str().unwrap().to_string();
    assert!(
        to.contains("/authorize?client_id=app1")
            && to.contains("scope=identify")
            && to.contains("response_type=code"),
        "{to}"
    );
    let state = cookie_of(&r, "cc_state").unwrap();
    assert!(to.contains(&format!("state={state}")) && state.len() >= 32);
    let set = r.headers()["set-cookie"].to_str().unwrap();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Lax"),
        "{set}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_callback_without_the_matching_state_is_refused() {
    let (hub, base) = rig(true).await;
    let c = client();
    // No cookie at all, and a cookie that differs: both are refused, so another site cannot force a sign-in.
    let a = c
        .get(format!("{base}/auth/callback?code=code-for-100&state=abc"))
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 400);
    let b = c
        .get(format!("{base}/auth/callback?code=code-for-100&state=abc"))
        .header("cookie", "cc_state=different")
        .send()
        .await
        .unwrap();
    assert_eq!(b.status(), 400);
    hub.shutdown().await;
}

#[tokio::test]
async fn only_listed_accounts_sign_in_and_each_sees_only_their_projects() {
    let (hub, base) = rig(true).await;
    assert_eq!(
        sign_in(&base, "999").await,
        Err(403),
        "an unlisted account is refused"
    );
    let owner = sign_in(&base, "100").await.unwrap();
    let operator = sign_in(&base, "200").await.unwrap();

    let (status, state) = get_json(&base, "/api/v1/state", &owner).await;
    assert_eq!(status, 200);
    assert_eq!(
        state["projects"].as_object().unwrap().len(),
        2,
        "the owner sees both projects"
    );
    let (_, state) = get_json(&base, "/api/v1/state", &operator).await;
    let names: Vec<&String> = state["projects"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["alpha"], "the operator sees only their project");

    assert_eq!(
        get_json(&base, "/api/v1/history?project=alpha&latest=1", &operator)
            .await
            .0,
        200
    );
    assert_eq!(
        get_json(&base, "/api/v1/history?project=beta&latest=1", &operator)
            .await
            .0,
        403
    );
    assert_eq!(
        get_json(&base, "/api/v1/history?project=beta&latest=1", &owner)
            .await
            .0,
        200
    );

    // Removing the person takes effect on their very next request.
    hub.call(|c, _| {
        (
            c.set_role(&human("100"), "alpha", &human("200"), None)
                .unwrap(),
            vec![],
        )
    })
    .await;
    assert_eq!(
        get_json(&base, "/api/v1/history?project=alpha&latest=1", &operator)
            .await
            .0,
        403
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn signing_out_ends_the_session_on_the_hub() {
    let (hub, base) = rig(true).await;
    let s = sign_in(&base, "100").await.unwrap();
    assert_eq!(
        get_json(&base, "/auth/me", &s).await,
        (200, json!({"user": "user100"}))
    );
    let out = client()
        .post(format!("{base}/auth/logout"))
        .header("cookie", format!("cc_session={s}"))
        .send()
        .await
        .unwrap();
    assert_eq!(out.status(), 204);
    assert_eq!(
        get_json(&base, "/api/v1/state", &s).await.0,
        401,
        "the old cookie no longer works"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn without_sign_in_set_up_the_routes_say_so_and_tokens_still_work() {
    let (hub, base) = rig(false).await;
    assert_eq!(
        client()
            .get(format!("{base}/auth/login"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // Asking with no credentials many times is not a failure and never blocks the address.
    for _ in 0..30 {
        assert_eq!(
            client()
                .get(format!("{base}/api/v1/state"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    let dir = std::env::temp_dir().join(format!("cc-login-tok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = Store::open(&dir.join("t.db"), None).unwrap();
    hub.shutdown().await;
    let mut core = HubCore::default();
    core.add_owner("100");
    let token = store.create_token("web:viewer", 0).unwrap();
    let hub = server::start(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        },
        core,
        store,
    )
    .await
    .unwrap();
    let r = client()
        .get(format!("http://{}/api/v1/state", hub.addr))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "a dashboard token sees everything");
    hub.shutdown().await;
}

#[tokio::test]
async fn many_people_signing_in_and_out_at_once_each_get_their_own_session() {
    let (hub, base) = rig(true).await;
    let tasks: Vec<_> = (0..40)
        .map(|_| {
            let base = base.clone();
            tokio::spawn(async move { sign_in(&base, "100").await.unwrap() })
        })
        .collect();
    let mut sessions = Vec::new();
    for t in tasks {
        sessions.push(t.await.unwrap());
    }
    let mut unique = sessions.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 40, "no two people share a session id");
    // Signing some out while others use theirs touches only the ones signed out.
    let (out, keep) = sessions.split_at(20);
    let outs: Vec<_> = out
        .iter()
        .map(|s| {
            let (base, s) = (base.clone(), s.clone());
            tokio::spawn(async move {
                client()
                    .post(format!("{base}/auth/logout"))
                    .header("cookie", format!("cc_session={s}"))
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();
    let reads: Vec<_> = keep
        .iter()
        .map(|s| {
            let (base, s) = (base.clone(), s.clone());
            tokio::spawn(async move { get_json(&base, "/api/v1/state", &s).await.0 })
        })
        .collect();
    for o in outs {
        assert_eq!(o.await.unwrap(), 204);
    }
    for r in reads {
        assert_eq!(
            r.await.unwrap(),
            200,
            "someone else signing out did not end this session"
        );
    }
    for s in out {
        assert_eq!(get_json(&base, "/auth/me", s).await.0, 401);
    }
    hub.shutdown().await;
}

#[tokio::test]
async fn odd_and_hostile_sign_in_requests_are_refused_cleanly_and_never_crash_the_hub() {
    let (hub, base) = rig(true).await;
    let c = client();
    let huge = "x".repeat(6_000);
    for url in [
        format!("{base}/auth/callback"),
        format!("{base}/auth/callback?code=only"),
        format!("{base}/auth/callback?state=only"),
        format!("{base}/auth/callback?code=%00%FF&state=%E2%80%AE"),
        format!("{base}/auth/callback?code={huge}&state={huge}"),
    ] {
        let r = c
            .get(&url)
            .header("cookie", "cc_state=x; cc_state=y; ;;; =")
            .send()
            .await
            .map(|r| r.status().as_u16());
        assert!(
            matches!(r, Ok(400 | 414 | 431 | 429)),
            "{:.60} gave {r:?}",
            url
        );
    }
    // Strange cookies never open anything, and never cause a server error.
    for cookie in [
        "cc_session=",
        "cc_session=%00",
        "cc_session=a; cc_session=b",
        &format!("cc_session={huge}"),
        "cc_session",
    ] {
        let r = c
            .get(format!("{base}/api/v1/state"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{:.40}", cookie);
    }
    assert_eq!(
        c.get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "the hub is fine"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn an_address_that_keeps_failing_to_sign_in_is_blocked_for_a_while() {
    let (hub, base) = rig(true).await;
    let c = client();
    for _ in 0..12 {
        c.get(format!("{base}/auth/callback?code=x&state=wrong"))
            .header("cookie", "cc_state=right")
            .send()
            .await
            .unwrap();
    }
    assert_eq!(
        sign_in(&base, "100").await,
        Err(429),
        "even a real sign-in waits once an address has failed too often"
    );
    hub.shutdown().await;
}
