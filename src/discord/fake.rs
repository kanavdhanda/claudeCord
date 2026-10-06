//! A stand-in Discord for tests and for `claudecord selftest`: the small part of Discord's REST API and live gateway that the
//! bridge uses, kept in memory. It records everything the bridge asks it to do (in `Log`) and lets a test inject gateway events.
//! It is not Discord and checks nothing about permissions, rate limits or message formats beyond what the bridge needs.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, Query, State,
        ws::{Message as WsMsg, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::{any, get, post, put},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Everything the stand-in Discord has been asked to do.
#[derive(Default)]
pub struct Log {
    pub next: u64,
    pub channels: Vec<Value>,
    /// Roles made in the servers: {id, name, guild, mentionable}.
    pub roles: Vec<Value>,
    /// Ids of the roles that were deleted.
    pub deleted_roles: Vec<String>,
    /// The servers the bot is in, as `/users/@me/guilds` returns them.
    pub guilds: Vec<Value>,
    pub webhooks: HashMap<String, String>, // webhook id -> channel id
    pub posts: Vec<Value>, // webhook posts: {username, content, thread_id, channel, file}
    pub messages: Vec<Value>, // bot messages: {channel, content, components, id}
    pub edits: Vec<Value>,
    pub reactions: Vec<Value>,
    /// Message ids deleted in bulk.
    pub deleted: Vec<String>,
    pub responses: Vec<Value>,
    pub commands: Option<Value>,
    /// What people said in each channel, as the "read the messages after" call returns it (a test fills this to stand in for messages sent
    /// while the bridge was not connected): channel id -> messages.
    pub history: HashMap<String, Vec<Value>>,
    pub gateway_connects: u32,
    pub identifies: u32,
    pub resumes: u32,
}

#[derive(Clone)]
pub struct Fake {
    pub log: Arc<Mutex<Log>>,
    pub events: broadcast::Sender<String>,
    pub addr: Arc<Mutex<String>>,
    pub kick: broadcast::Sender<()>,
    /// How many of the next "who am I" calls answer with an error, to stand in for Discord being down when the bridge starts.
    pub me_failures: Arc<std::sync::atomic::AtomicUsize>,
    /// When set, a request to resume a session is refused (as Discord does when it has forgotten the session), so the bridge starts a fresh
    /// one and has to read back what it missed.
    pub reject_resume: Arc<std::sync::atomic::AtomicBool>,
}

impl Fake {
    fn id(&self) -> String {
        let mut l = self.log.lock().unwrap();
        l.next += 1;
        (1000 + l.next).to_string()
    }
}

pub async fn start_fake() -> (Fake, String) {
    let (events, _) = broadcast::channel(64);
    let (kick, _) = broadcast::channel(4);
    let log = Log {
        guilds: vec![
            json!({"id": "g1", "name": "Test server"}),
            json!({"id": "g2", "name": "Second server"}),
        ],
        ..Log::default()
    };
    let fake = Fake {
        log: Arc::new(Mutex::new(log)),
        events,
        addr: Arc::default(),
        kick,
        me_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        reject_resume: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    async fn me(State(f): State<Fake>) -> Result<Json<Value>, axum::http::StatusCode> {
        let down = f
            .me_failures
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok();
        if down {
            return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(Json(json!({"id": "app1", "owner": {"id": "1"}})))
    }
    /// Who the bot is, from its token: its user id is the first part of the token, as with real Discord.
    async fn user_me(
        headers: axum::http::HeaderMap,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bot "))
            .ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
        if auth.starts_with("bad") {
            return Err(axum::http::StatusCode::UNAUTHORIZED);
        }
        let id = crate::discord::perms::app_id_from_token(auth).unwrap_or_else(|| "app1".into());
        Ok(Json(
            json!({"id": id, "username": format!("bot-{id}"), "bot": true}),
        ))
    }
    async fn my_guilds(State(f): State<Fake>) -> Json<Value> {
        Json(Value::Array(f.log.lock().unwrap().guilds.clone()))
    }
    async fn gw(State(f): State<Fake>) -> Json<Value> {
        Json(json!({"url": format!("ws://{}/gateway", f.addr.lock().unwrap())}))
    }
    async fn list(State(f): State<Fake>, Path(g): Path<String>) -> Json<Value> {
        let all = f.log.lock().unwrap().channels.clone();
        // A channel made without a server (older tests) counts as being in every server.
        Json(Value::Array(
            all.into_iter()
                .filter(|c| c["guild"].as_str().is_none_or(|x| x == g))
                .collect(),
        ))
    }
    async fn roles(State(f): State<Fake>, Path(g): Path<String>) -> Json<Value> {
        let all = f.log.lock().unwrap().roles.clone();
        Json(Value::Array(
            all.into_iter()
                .filter(|r| r["guild"] == g.as_str())
                .collect(),
        ))
    }
    async fn mk_role(
        State(f): State<Fake>,
        Path(g): Path<String>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        let id = f.id();
        let r = json!({"id": id, "name": b["name"], "guild": g, "mentionable": b["mentionable"]});
        f.log.lock().unwrap().roles.push(r.clone());
        Json(r)
    }
    async fn del_role(
        State(f): State<Fake>,
        Path((_g, id)): Path<(String, String)>,
    ) -> Json<Value> {
        let mut l = f.log.lock().unwrap();
        l.roles.retain(|r| r["id"] != id.as_str());
        l.deleted_roles.push(id);
        Json(json!({}))
    }
    async fn get_channel(
        State(f): State<Fake>,
        Path(id): Path<String>,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        f.log
            .lock()
            .unwrap()
            .channels
            .iter()
            .find(|c| c["id"] == id.as_str())
            .cloned()
            .map(Json)
            .ok_or(axum::http::StatusCode::NOT_FOUND)
    }
    async fn patch_channel(
        State(f): State<Fake>,
        Path(id): Path<String>,
        Json(b): Json<Value>,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        let mut l = f.log.lock().unwrap();
        let c = l
            .channels
            .iter_mut()
            .find(|c| c["id"] == id.as_str())
            .ok_or(axum::http::StatusCode::NOT_FOUND)?;
        if let Some(t) = b.get("topic") {
            c["topic"] = t.clone();
        }
        Ok(Json(c.clone()))
    }
    async fn mk_channel(
        State(f): State<Fake>,
        Path(g): Path<String>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        let id = f.id();
        let c = json!({"id": id, "name": b["name"], "type": 0, "guild": g, "topic": b["topic"]});
        f.log.lock().unwrap().channels.push(c.clone());
        Json(c)
    }
    async fn mk_hook(State(f): State<Fake>, Path(ch): Path<String>) -> Json<Value> {
        let id = f.id();
        f.log.lock().unwrap().webhooks.insert(id.clone(), ch);
        Json(json!({"id": id, "token": "tok"}))
    }
    async fn hook_post(
        State(f): State<Fake>,
        Path((id, _tok)): Path<(String, String)>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Json<Value> {
        let mid = f.id();
        let multipart = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("multipart"));
        let text = String::from_utf8_lossy(&body).to_string();
        let notify = serde_json::from_slice::<Value>(&body)
            .ok()
            .map(|v| v["allowed_mentions"]["users"].clone())
            .unwrap_or(Value::Null);
        let (username, content, file) = if multipart {
            let payload = text
                .split("name=\"payload_json\"")
                .nth(1)
                .and_then(|r| r.split("\r\n\r\n").nth(1))
                .and_then(|r| r.split("\r\n--").next())
                .unwrap_or("{}");
            let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
            let fname = text
                .split("filename=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap_or("")
                .to_string();
            (
                v["username"].as_str().unwrap_or("").to_string(),
                v["content"].as_str().unwrap_or("").to_string(),
                Some(fname),
            )
        } else {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            (
                v["username"].as_str().unwrap_or("").to_string(),
                v["content"].as_str().unwrap_or("").to_string(),
                None,
            )
        };
        let mut l = f.log.lock().unwrap();
        let channel = l.webhooks.get(&id).cloned().unwrap_or_default();
        l.posts.push(json!({"username": username, "content": content, "thread_id": q.get("thread_id"), "channel": channel, "file": file, "id": mid, "notify": notify}));
        Json(json!({"id": mid}))
    }
    async fn mk_thread(State(f): State<Fake>, Json(b): Json<Value>) -> Json<Value> {
        let id = f.id();
        let _ = b;
        Json(json!({"id": id}))
    }
    async fn send(
        State(f): State<Fake>,
        Path(ch): Path<String>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        let id = f.id();
        f.log.lock().unwrap().messages.push(json!({"channel": ch, "content": b["content"], "components": b["components"], "id": id}));
        Json(json!({"id": id}))
    }
    async fn edit(
        State(f): State<Fake>,
        Path((ch, m)): Path<(String, String)>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        f.log.lock().unwrap().edits.push(json!({"channel": ch, "message": m, "content": b["content"], "components": b["components"]}));
        Json(json!({}))
    }
    async fn react(
        State(f): State<Fake>,
        Path((ch, m, e)): Path<(String, String, String)>,
    ) -> Json<Value> {
        f.log
            .lock()
            .unwrap()
            .reactions
            .push(json!({"channel": ch, "message": m, "emoji": e}));
        Json(json!({}))
    }
    async fn unreact(
        State(f): State<Fake>,
        Path((ch, m, e)): Path<(String, String, String)>,
    ) -> Json<Value> {
        f.log.lock().unwrap().reactions.retain(|x| {
            !(x["channel"] == ch.as_str() && x["message"] == m.as_str() && x["emoji"] == e.as_str())
        });
        Json(json!({}))
    }
    async fn bulk_delete(
        State(f): State<Fake>,
        Path(ch): Path<String>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        let ids: Vec<String> = b["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_str().map(String::from))
            .collect();
        let mut l = f.log.lock().unwrap();
        if let Some(h) = l.history.get_mut(&ch) {
            h.retain(|m| !ids.iter().any(|i| m["id"] == i.as_str()));
        }
        l.deleted.extend(ids);
        Json(json!({}))
    }
    async fn respond(
        State(f): State<Fake>,
        Path((id, _t)): Path<(String, String)>,
        Json(b): Json<Value>,
    ) -> Json<Value> {
        f.log
            .lock()
            .unwrap()
            .responses
            .push(json!({"interaction": id, "content": b["data"]["content"], "choices": b["data"]["choices"], "type": b["type"]}));
        Json(json!({}))
    }
    async fn cmds(State(f): State<Fake>, Json(b): Json<Value>) -> Json<Value> {
        f.log.lock().unwrap().commands = Some(b);
        Json(json!([]))
    }
    async fn list_messages(
        State(f): State<Fake>,
        axum::extract::Path(channel): axum::extract::Path<String>,
        axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let after: u64 = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
        let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50);
        let log = f.log.lock().unwrap();
        let mut found: Vec<Value> = log
            .history
            .get(&channel)
            .into_iter()
            .flatten()
            .filter(|m| {
                m["id"]
                    .as_str()
                    .and_then(|i| i.parse::<u64>().ok())
                    .is_some_and(|i| i > after)
            })
            .cloned()
            .collect();
        // Discord answers newest first.
        found.sort_by_key(|m| {
            std::cmp::Reverse(
                m["id"]
                    .as_str()
                    .and_then(|i| i.parse::<u64>().ok())
                    .unwrap_or(0),
            )
        });
        found.truncate(limit);
        Json(Value::Array(found))
    }
    async fn file() -> &'static str {
        "attachment bytes"
    }
    async fn ws(State(f): State<Fake>, up: WebSocketUpgrade) -> impl IntoResponse {
        up.on_upgrade(move |s| gateway(f, s))
    }
    async fn gateway(f: Fake, mut s: WebSocket) {
        f.log.lock().unwrap().gateway_connects += 1;
        let mut events = f.events.subscribe();
        let mut kick = f.kick.subscribe();
        let _ = s
            .send(WsMsg::Text(
                json!({"op": 10, "d": {"heartbeat_interval": 150}})
                    .to_string()
                    .into(),
            ))
            .await;
        loop {
            tokio::select! {
                m = s.recv() => {
                    let Some(Ok(WsMsg::Text(t))) = m else { return };
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    match v["op"].as_u64() {
                        Some(2) => {
                            f.log.lock().unwrap().identifies += 1;
                            let _ = s.send(WsMsg::Text(json!({"op": 0, "s": 1, "t": "READY", "d": {"session_id": "sess", "resume_gateway_url": format!("ws://{}/gateway", f.addr.lock().unwrap())}}).to_string().into())).await;
                        }
                        Some(6) if f.reject_resume.load(std::sync::atomic::Ordering::SeqCst) => {
                            // "I do not know that session": the bridge must start a new one.
                            let _ = s.send(WsMsg::Text(json!({"op": 9, "d": false}).to_string().into())).await;
                        }
                        Some(6) => {
                            f.log.lock().unwrap().resumes += 1;
                            // The real gateway confirms a resume with this event.
                            let _ = s.send(WsMsg::Text(json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}).to_string().into())).await;
                        }
                        Some(1) => { let _ = s.send(WsMsg::Text(json!({"op": 11}).to_string().into())).await; }
                        _ => {}
                    }
                }
                e = events.recv() => { if let Ok(e) = e { let _ = s.send(WsMsg::Text(e.into())).await; } }
                _ = kick.recv() => { return; }
            }
        }
    }
    let app = Router::new()
        .route("/api/oauth2/applications/@me", get(me))
        .route("/api/gateway/bot", get(gw))
        .route("/api/users/@me", get(user_me))
        .route("/api/users/@me/guilds", get(my_guilds))
        .route("/api/guilds/{g}/channels", get(list).post(mk_channel))
        .route("/api/guilds/{g}/roles", get(roles).post(mk_role))
        .route(
            "/api/guilds/{g}/roles/{id}",
            axum::routing::delete(del_role),
        )
        .route("/api/channels/{id}", get(get_channel).patch(patch_channel))
        .route("/api/channels/{id}/webhooks", post(mk_hook))
        .route("/api/webhooks/{id}/{tok}", post(hook_post))
        .route("/api/channels/{id}/threads", post(mk_thread))
        .route("/api/channels/{id}/messages", post(send).get(list_messages))
        .route("/api/channels/{id}/messages/bulk-delete", post(bulk_delete))
        .route("/api/channels/{c}/messages/{m}", any(edit))
        .route(
            "/api/channels/{c}/messages/{m}/reactions/{e}/@me",
            put(react).delete(unreact),
        )
        .route("/api/interactions/{id}/{t}/callback", post(respond))
        .route("/api/applications/{a}/guilds/{g}/commands", put(cmds))
        .route("/files/{name}", get(file))
        .route("/gateway", get(ws))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    *fake.addr.lock().unwrap() = addr.clone();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (fake, addr)
}
