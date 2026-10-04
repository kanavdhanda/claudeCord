//! "Sign in with Discord" for the dashboard, the standard OAuth2 code flow. The hub sends the browser to Discord, Discord sends
//! it back with a one-time code, and the hub swaps the code for the person's Discord account id (scope `identify` only, so
//! nothing else about them is asked for). The id must be on some project's list; the dashboard then shows only the projects
//! that account has a role in, checked on every request so a removed person loses access at once.
//!
//! Safety: a random `state` kept in a short cookie stops a sign-in being forced from another site; cookies are HttpOnly and
//! SameSite=Lax (and Secure on https); the session id is random and kept only in the hub's memory (a restart signs everyone
//! out, which is why it is cheap and safe); failures count against the same per-address block as bad tokens.

use super::web::secure as secure_headers;
use super::{AppState, Oauth};
use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

/// Who is signed in: the browser's session id maps to this until it expires.
pub(crate) struct Session {
    pub id: String,
    pub name: String,
    expires: i64,
}

pub(crate) type Sessions = std::sync::Arc<std::sync::Mutex<HashMap<String, Session>>>;

const SESSION_MS: i64 = 8 * 3600 * 1000;
// ponytail: sessions live in memory with a fixed cap; move them into the database if the dashboard gets many thousands of viewers.
const MAX_SESSIONS: usize = 10_000;

fn random_hex() -> String {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("the system has a random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Percent-encodes everything but unreserved characters, for query strings and form bodies.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The value of a cookie by name.
pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|p| {
            p.trim()
                .strip_prefix(&format!("{name}="))
                .map(str::to_string)
        })
}

/// Compares two secrets without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn set_cookie(r: &mut Response, name: &str, value: &str, path: &str, max_age: i64, oauth: &Oauth) {
    let secure = if oauth.redirect_uri.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let c =
        format!("{name}={value}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={max_age}{secure}");
    r.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&c).expect("cookie text is plain"),
    );
}

fn page(status: StatusCode, text: &str) -> Response {
    secure_headers(
        (status, text.to_string()).into_response(),
        "text/plain; charset=utf-8",
        "no-store",
    )
}

/// Who a session cookie belongs to, if it is a live session.
pub(crate) fn who(st: &AppState, headers: &HeaderMap) -> Option<(String, String)> {
    let id = cookie(headers, "cc_session")?;
    let mut all = st.sessions.lock().expect("lock");
    match all.get(&id) {
        Some(s) if s.expires > crate::now_ms() => Some((s.id.clone(), s.name.clone())),
        Some(_) => {
            all.remove(&id);
            None
        }
        None => None,
    }
}

/// Sends the browser to Discord.
pub(super) async fn start(State(st): State<AppState>) -> Response {
    let Some(o) = st.cfg.oauth.clone() else {
        return page(
            StatusCode::NOT_FOUND,
            "sign-in with Discord is not set up on this hub",
        );
    };
    let state = random_hex();
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope=identify&state={state}&prompt=none",
        o.authorize_url,
        pct(&o.client_id),
        pct(&o.redirect_uri)
    );
    let mut r = Redirect::to(&url).into_response();
    set_cookie(&mut r, "cc_state", &state, "/auth", 600, &o);
    secure_headers(r, "text/plain", "no-store")
}

/// Swaps Discord's one-time code for the person's account.
async fn identify(o: &Oauth, code: &str) -> Result<(String, String), String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let body = format!(
        "client_id={}&client_secret={}&grant_type=authorization_code&code={}&redirect_uri={}",
        pct(&o.client_id),
        pct(&o.client_secret),
        pct(code),
        pct(&o.redirect_uri)
    );
    let tok: Value = http
        .post(format!("{}/oauth2/token", o.api_base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let access = tok["access_token"].as_str().ok_or("no access token")?;
    let me: Value = http
        .get(format!("{}/users/@me", o.api_base))
        .bearer_auth(access)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let id = me["id"]
        .as_str()
        .filter(|i| !i.is_empty() && i.len() <= 20 && i.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("no valid account id")?;
    let name = me["global_name"]
        .as_str()
        .or(me["username"].as_str())
        .unwrap_or("someone");
    Ok((
        id.to_string(),
        name.chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect::<String>(),
    ))
}

/// Where Discord sends the person back to.
pub(super) async fn callback(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(o) = st.cfg.oauth.clone() else {
        return page(
            StatusCode::NOT_FOUND,
            "sign-in with Discord is not set up on this hub",
        );
    };
    let ip = peer.ip().to_string();
    let now = crate::now_ms();
    if st.failures.lock().expect("lock").blocked(&ip, now as f64) {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts, wait a minute",
        );
    }
    let refuse = |status, text: &str| {
        st.failures.lock().expect("lock").fail(&ip, now as f64);
        page(status, text)
    };
    let (Some(code), Some(state), Some(mine)) =
        (q.get("code"), q.get("state"), cookie(&headers, "cc_state"))
    else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the sign-in expired or did not start here, open the dashboard and try again",
        );
    };
    if !same(state, &mine) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the sign-in did not start here, open the dashboard and try again",
        );
    }
    let Ok((id, name)) = identify(&o, code).await else {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "Discord did not confirm the sign-in",
        );
    };
    let listed = {
        let who = id.clone();
        // An owner has a role everywhere, including in a project name nobody uses; anyone else needs a role in a real project.
        st.handle
            .call(move |c, _| {
                (
                    c.role_of("", &who).is_some()
                        || c.projects().iter().any(|p| c.role_of(p, &who).is_some()),
                    vec![],
                )
            })
            .await
            .unwrap_or(false)
    };
    if !listed {
        return refuse(
            StatusCode::FORBIDDEN,
            "this Discord account is not on any project's list",
        );
    }
    let sid = random_hex();
    {
        let mut all = st.sessions.lock().expect("lock");
        all.retain(|_, s| s.expires > now);
        if all.len() >= MAX_SESSIONS {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many people are signed in, try again later",
            );
        }
        all.insert(
            sid.clone(),
            Session {
                id,
                name,
                expires: now + SESSION_MS,
            },
        );
    }
    let mut r = Redirect::to("/").into_response();
    set_cookie(&mut r, "cc_session", &sid, "/", SESSION_MS / 1000, &o);
    set_cookie(&mut r, "cc_state", "", "/auth", 0, &o);
    secure_headers(r, "text/plain", "no-store")
}

/// Who is signed in, for the page to show.
pub(super) async fn me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match who(&st, &headers) {
        Some((_, name)) => secure_headers(
            axum::Json(serde_json::json!({ "user": name })).into_response(),
            "application/json",
            "no-store",
        ),
        None => page(StatusCode::UNAUTHORIZED, "not signed in"),
    }
}

/// Signs out: the session is forgotten here, not just in the browser.
pub(super) async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, "cc_session") {
        st.sessions.lock().expect("lock").remove(&id);
    }
    let mut r = page(StatusCode::NO_CONTENT, "");
    if let Some(o) = &st.cfg.oauth {
        set_cookie(&mut r, "cc_session", "", "/", 0, o);
    }
    r
}
