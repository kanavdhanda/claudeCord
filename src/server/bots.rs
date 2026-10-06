//! The dashboard's bot and workspace endpoints: saving a Discord bot token (sealed, never shown again), listing the servers and channels
//! the bot can reach, choosing or making the channel a project posts to, and starting another agent on one of the account's machines.
//! Every route works on the signed-in account's own bots and projects: a bot id from another account finds nothing.

use super::gateway::{Gateway, account_of, err, json_reply};
use crate::control::seal;
use crate::discord::api::Rest;
use crate::hub::controls::MoveError;
use crate::hub::{Chat, Effect, Human};
use crate::protocol::{AdapterId, AgentSpec, is_slug};
use crate::sync::Lock;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::json;

/// Most bots one account may save.
const MAX_BOTS: usize = 10;

/// The routes this module adds to the gateway.
pub(crate) fn routes() -> Router<Gateway> {
    Router::new()
        .route("/api/v1/bots", get(list_bots).post(add_bot))
        .route("/api/v1/bots/{bot}", delete(remove_bot))
        .route("/api/v1/bots/{bot}/guilds", get(guilds))
        .route(
            "/api/v1/bots/{bot}/guilds/{guild}/channels",
            get(channels).post(make_channel),
        )
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/projects/{project}/target", put(set_target))
        .route("/api/v1/spawn", post(spawn))
        .route(
            "/api/v1/projects/{project}/agents/{name}/move",
            post(move_agent),
        )
}

/// What Discord says the bot's permissions are in one server (a number, as text), or None if it does not say.
fn granted(g: &serde_json::Value) -> Option<u64> {
    match &g["permissions"] {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// What is missing for the bot to do its job in a server, in words, or None if it can (or Discord does not say, as the stand-in does not).
fn guild_problem(guilds: &[serde_json::Value], guild: &str) -> Option<String> {
    let Some(g) = guilds.iter().find(|g| g["id"] == guild) else {
        return Some("The bot is not in that Discord server (it may have been removed). Add it again with the invite link on the Discord bots page.".into());
    };
    let missing = granted(g)
        .map(crate::discord::perms::missing_permissions)
        .unwrap_or_default();
    (!missing.is_empty()).then(|| {
        format!(
            "In \"{}\" the bot is missing: {}. Invite it again with the link on the Discord bots page. If Discord keeps the old permissions, remove the bot from the server first and add it back.",
            g["name"].as_str().unwrap_or("that server"),
            missing.join(", ")
        )
    })
}

/// The servers a bot is in, as Discord said within the last 20 seconds (asked again after that). None if Discord cannot be asked.
pub(crate) async fn bot_guilds_cached(
    gw: &Gateway,
    tenant: &str,
    bot: &str,
) -> Option<Vec<serde_json::Value>> {
    let key = format!("{tenant}/{bot}");
    let now = crate::now_ms();
    if let Some((at, g)) = gw.guild_cache.locked().get(&key)
        && now - at < 20_000
    {
        return Some(g.clone());
    }
    let (rest, _) = rest_for(gw, tenant, bot).ok()?;
    let g = rest.my_guilds().await.ok()?;
    gw.guild_cache.locked().insert(key, (now, g.clone()));
    Some(g)
}

/// Why a placed project cannot work right now (its bot was removed from the server, or lacks a permission), or None if it can or that cannot be
/// told. This is what the dashboard shows, and what `claudecord start` waits on.
pub(crate) async fn project_problem(
    gw: &Gateway,
    tenant: &str,
    p: &crate::control::Placement,
) -> Option<String> {
    let guilds = bot_guilds_cached(gw, tenant, &p.bot).await?;
    guild_problem(&guilds, &p.guild)
}

/// Whether the account has at least one bot in a server with every permission it needs. Err says what to do.
pub(crate) async fn account_ready(gw: &Gateway, tenant: &str) -> Result<(), String> {
    let bots = gw.control.bots(tenant).unwrap_or_default();
    if bots.is_empty() {
        return Err(
            "add a Discord bot first (the Discord bots page): its token is checked with Discord"
                .into(),
        );
    }
    let mut why = String::new();
    for (id, _, name) in &bots {
        let Ok((rest, _)) = rest_for(gw, tenant, id) else {
            continue;
        };
        let Ok(guilds) = rest.my_guilds().await else {
            continue;
        };
        if guilds.is_empty() {
            why = format!(
                "{name} is not in any Discord server yet. Invite it with the link on the Discord bots page."
            );
            continue;
        }
        let works_somewhere = guilds
            .iter()
            .filter_map(|g| g["id"].as_str())
            .any(|i| guild_problem(&guilds, i).is_none());
        if works_somewhere {
            return Ok(());
        }
        why = guild_problem(&guilds, guilds[0]["id"].as_str().unwrap_or("")).unwrap_or(why);
    }
    Err(if why.is_empty() {
        "none of your bots could be checked with Discord just now; try again in a moment".into()
    } else {
        why
    })
}

/// Marks a channel as this service's, in its topic, or says it is already another service's. Without this, two copies of claudeCord (a real one
/// and a test one) given the same bot and the same channel would each answer every message.
async fn claim_channel(rest: &Rest, hub: &str, channel: &str) -> Result<(), Box<Response>> {
    static MARK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[claudeCord:([0-9a-f]+)\]").expect("mark")
    });
    let info = rest.channel(channel).await.map_err(|_| {
        Box::new(err(
            StatusCode::BAD_GATEWAY,
            "Discord did not answer about that channel (can the bot see it?)",
        ))
    })?;
    let topic = info["topic"].as_str().unwrap_or("").to_string();
    match MARK.captures(&topic) {
        Some(c) if &c[1] == hub => Ok(()),
        Some(c) => Err(Box::new(err(
            StatusCode::CONFLICT,
            &format!(
                "That channel is already used by another claudeCord service ({}), probably your other or test copy. Choose a different channel, or use a separate bot for testing. (To release it, remove the [claudeCord:{}] text from the channel's topic.)",
                &c[1], &c[1]
            ),
        ))),
        None => {
            let marked: String = format!("[claudeCord:{hub}] {topic}")
                .chars()
                .take(1000)
                .collect();
            rest.set_channel_topic(channel, &marked).await.map_err(|_| {
                Box::new(err(
                    StatusCode::BAD_GATEWAY,
                    "Discord would not let the bot mark that channel (does it have Manage Channels?)",
                ))
            })
        }
    }
}

/// Discord ids are digits only; anything else is refused before it goes into a URL.
fn is_snowflake(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// A Discord client for one of the account's bots, with the token opened for this call only. Err is the answer to send.
fn rest_for(gw: &Gateway, tenant: &str, bot: &str) -> Result<(Rest, String), Box<Response>> {
    let sealed = gw
        .control
        .sealed_bot(tenant, bot)
        .ok()
        .flatten()
        .ok_or_else(|| Box::new(err(StatusCode::NOT_FOUND, "no such bot")))?;
    let app = gw
        .control
        .bots(tenant)
        .unwrap_or_default()
        .into_iter()
        .find(|(id, _, _)| id == bot)
        .map(|(_, a, _)| a)
        .ok_or_else(|| Box::new(err(StatusCode::NOT_FOUND, "no such bot")))?;
    let token = seal::open(gw.keys.as_ref(), tenant, &app, &sealed).ok_or_else(|| {
        Box::new(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "this bot's saved token cannot be opened; save it again",
        ))
    })?;
    Ok((Rest::new(&gw.discord.api_base, &token), app))
}

/// The account's bots. Never includes a token, sealed or not.
async fn list_bots(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let bots: Vec<_> = gw
        .control
        .bots(&a.id)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, app, name)| {
            json!({"id": id, "app_id": app, "name": name, "invite_url": crate::discord::perms::invite_url(&app)})
        })
        .collect();
    json_reply(StatusCode::OK, json!(bots))
}

#[derive(Deserialize)]
struct AddBot {
    token: String,
}

/// Saves a bot token. Discord is asked who the token belongs to, which proves it is real; then it is sealed and the bot's bridge starts.
async fn add_bot(State(gw): State<Gateway>, headers: HeaderMap, Json(b): Json<AddBot>) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let token = b.token.trim().to_string();
    if token.len() > 200 || crate::discord::perms::app_id_from_token(&token).is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "that does not look like a Discord bot token",
        );
    }
    if gw.control.bots(&a.id).map_or(0, |b| b.len()) >= MAX_BOTS {
        return err(
            StatusCode::CONFLICT,
            "this account already has the most bots it may save",
        );
    }
    let rest = Rest::new(&gw.discord.api_base, &token);
    let Ok(user) = rest.bot_user().await else {
        return err(StatusCode::BAD_REQUEST, "Discord did not accept that token");
    };
    let app_id = user["id"].as_str().unwrap_or("").to_string();
    if !is_snowflake(&app_id) {
        return err(
            StatusCode::BAD_GATEWAY,
            "Discord's answer was not understood",
        );
    }
    let name: String = user["username"]
        .as_str()
        .unwrap_or("bot")
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let Some(sealed) = seal::seal(gw.keys.as_ref(), &a.id, &app_id, &token) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store the token safely",
        );
    };
    let now = crate::now_ms();
    let Ok(id) = gw.control.save_bot(&a.id, &app_id, &name, &sealed, now) else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "could not save the bot");
    };
    if let Ok(Some(t)) = gw.registry.hub_of(&a.id) {
        gw.registry.start_bridge(&t, &id);
    }
    crate::info!("gateway", "account {} saved bot {id}", a.id);
    json_reply(
        StatusCode::OK,
        json!({"id": id, "app_id": app_id, "name": name, "invite_url": crate::discord::perms::invite_url(&app_id)}),
    )
}

/// Forgets a bot: its bridge stops and its token is deleted. Projects that posted through it become unplaced.
async fn remove_bot(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(bot): Path<String>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if let Ok(Some(t)) = gw.registry.hub_of(&a.id) {
        gw.registry.stop_bridge(&t, &bot);
    }
    let gone = gw.control.delete_bot(&a.id, &bot).unwrap_or(false);
    json_reply(StatusCode::OK, json!({"removed": gone}))
}

/// The servers the bot has been added to.
async fn guilds(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(bot): Path<String>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let (rest, _) = match rest_for(&gw, &a.id, &bot) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    match rest.my_guilds().await {
        Ok(g) => json_reply(
            StatusCode::OK,
            json!(
                g.iter()
                    .map(|x| {
                        let missing = granted(x)
                            .map(crate::discord::perms::missing_permissions)
                            .unwrap_or_default();
                        json!({"id": x["id"], "name": x["name"], "missing": missing, "ok": missing.is_empty()})
                    })
                    .collect::<Vec<_>>()
            ),
        ),
        Err(_) => err(StatusCode::BAD_GATEWAY, "Discord did not answer"),
    }
}

/// The text channels of one of the bot's servers.
async fn channels(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path((bot, guild)): Path<(String, String)>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !is_snowflake(&guild) && !guild.starts_with('g') {
        return err(StatusCode::BAD_REQUEST, "not a server id");
    }
    let (rest, _) = match rest_for(&gw, &a.id, &bot) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    match rest.guild_channels(&guild).await {
        Ok(c) => json_reply(
            StatusCode::OK,
            json!(
                c.iter()
                    .filter(|x| x["type"] == 0)
                    .map(|x| json!({"id": x["id"], "name": x["name"]}))
                    .collect::<Vec<_>>()
            ),
        ),
        Err(_) => err(
            StatusCode::BAD_GATEWAY,
            "Discord did not answer (is the bot in that server?)",
        ),
    }
}

#[derive(Deserialize)]
struct MakeChannel {
    name: String,
}

/// Makes a new text channel in one of the bot's servers.
async fn make_channel(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path((bot, guild)): Path<(String, String)>,
    Json(b): Json<MakeChannel>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !is_snowflake(&guild) && !guild.starts_with('g') {
        return err(StatusCode::BAD_REQUEST, "not a server id");
    }
    if !is_slug(&b.name) {
        return err(
            StatusCode::BAD_REQUEST,
            "a channel name uses letters, digits, dots, dashes and underscores",
        );
    }
    let (rest, _) = match rest_for(&gw, &a.id, &bot) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    match rest.my_guilds().await {
        Ok(g) => {
            if let Some(why) = guild_problem(&g, &guild) {
                return err(StatusCode::CONFLICT, &why);
            }
        }
        Err(_) => return err(StatusCode::BAD_GATEWAY, "Discord did not answer"),
    }
    let hub = gw.control.hub_id().unwrap_or_default();
    match rest
        .create_channel(
            &guild,
            &b.name.to_lowercase(),
            &format!("claudeCord project [claudeCord:{hub}]"),
        )
        .await
    {
        Ok(id) => json_reply(
            StatusCode::OK,
            json!({"id": id, "name": b.name.to_lowercase()}),
        ),
        Err(_) => err(
            StatusCode::BAD_GATEWAY,
            "Discord would not make the channel (does the bot have Manage Channels?)",
        ),
    }
}

/// The account's projects, each with where it posts (or that it has not been placed yet), its agents, and the machines.
async fn projects(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let Ok(t) = gw.registry.hub(&a) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub could not start");
    };
    let names: Vec<String> = t
        .state
        .handle
        .read(|c, _| c.projects())
        .await
        .unwrap_or_default();
    let placed = gw.control.targets(&a.id).unwrap_or_default();
    let mut all: Vec<String> = names;
    for p in &placed {
        if !all.contains(&p.project) {
            all.push(p.project.clone());
        }
    }
    all.sort();
    // Anything that stops a placed project working right now (the bot was removed from the server, or lacks a permission), to show.
    let mut problems: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in &placed {
        if let Some(why) = project_problem(&gw, &a.id, p).await {
            problems.insert(p.project.clone(), why);
        }
    }
    let rows: Vec<_> = all
        .iter()
        .map(|p| {
            let t = placed.iter().find(|x| &x.project == p);
            json!({
                "project": p,
                "placed": t.is_some(),
                "bot": t.map(|t| t.bot.clone()),
                "guild": t.map(|t| t.guild.clone()),
                "channel": t.map(|t| t.channel.clone()),
                "guild_name": t.map(|t| t.guild_name.clone()),
                "channel_name": t.map(|t| t.channel_name.clone()),
                "problem": problems.get(p),
            })
        })
        .collect();
    json_reply(StatusCode::OK, json!(rows))
}

#[derive(Deserialize)]
struct SetTarget {
    bot: String,
    guild: String,
    channel: String,
}

/// Chooses where a project posts. The channel must really be in that server of that bot (Discord is asked), so a project can never be
/// pointed at a channel the bot cannot reach, nor at one the account has no bot in.
async fn set_target(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(b): Json<SetTarget>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !is_slug(&project) {
        return err(
            StatusCode::BAD_REQUEST,
            "a project name uses letters, digits, dots, dashes and underscores",
        );
    }
    if !(is_snowflake(&b.guild) || b.guild.starts_with('g')) || !is_snowflake(&b.channel) {
        return err(StatusCode::BAD_REQUEST, "not a server or channel id");
    }
    let (rest, _) = match rest_for(&gw, &a.id, &b.bot) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let Ok(my_guilds) = rest.my_guilds().await else {
        return err(StatusCode::BAD_GATEWAY, "Discord did not answer");
    };
    if let Some(why) = guild_problem(&my_guilds, &b.guild) {
        return err(StatusCode::CONFLICT, &why);
    }
    let Ok(list) = rest.guild_channels(&b.guild).await else {
        return err(
            StatusCode::BAD_GATEWAY,
            "Discord did not answer (is the bot in that server?)",
        );
    };
    let Some(chan) = list
        .iter()
        .find(|c| c["id"] == b.channel.as_str() && c["type"] == 0)
    else {
        return err(StatusCode::NOT_FOUND, "that channel is not in that server");
    };
    let channel_name = chan["name"].as_str().unwrap_or("").to_string();
    let hub = gw.control.hub_id().unwrap_or_default();
    if let Err(r) = claim_channel(&rest, &hub, &b.channel).await {
        return *r;
    }
    // The server's name is only for showing; if Discord will not say, the id stands in.
    let guild_name = my_guilds
        .iter()
        .find(|g| g["id"] == b.guild.as_str())
        .and_then(|g| g["name"].as_str().map(String::from))
        .unwrap_or_else(|| b.guild.clone());
    // A project that was posting through another bot of the same account stops there and starts here.
    let before = gw.control.target(&a.id, &project).ok().flatten();
    let place = crate::control::Placement {
        project: project.clone(),
        bot: b.bot.clone(),
        guild: b.guild.clone(),
        channel: b.channel.clone(),
        guild_name,
        channel_name,
    };
    if gw.control.set_target(&a.id, &place).is_err() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the place",
        );
    }
    if let Ok(Some(t)) = gw.registry.hub_of(&a.id) {
        gw.registry.start_bridge(&t, &b.bot);
        if let Some(old) = before.map(|p| p.bot)
            && old != b.bot
        {
            gw.registry.start_bridge(&t, &old);
        }
        // The project exists in the hub from now on, so its channel gets its status message.
        let p = project.clone();
        t.state
            .handle
            .call(move |_, _| {
                (
                    (),
                    vec![
                        Effect::Chat(Chat::EnsureProject(p.clone())),
                        Effect::Chat(Chat::RefreshStatus(p)),
                    ],
                )
            })
            .await;
    }
    json_reply(StatusCode::OK, json!({"ok": true}))
}

#[derive(Deserialize)]
struct MoveAsk {
    to: String,
}

/// `POST /api/v1/projects/{project}/agents/{name}/move`: the signed-in owner moves a running agent to another of their projects, in place.
async fn move_agent(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    Path((project, name)): Path<(String, String)>,
    Json(b): Json<MoveAsk>,
) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !is_slug(&project) || !is_slug(&b.to) {
        return err(
            StatusCode::BAD_REQUEST,
            "names use letters, digits, dots, dashes and underscores",
        );
    }
    // Where the agent goes must show in Discord: a project nobody has placed has no channel to post in.
    if gw.control.target(&a.id, &b.to).ok().flatten().is_none() {
        return err(
            StatusCode::CONFLICT,
            &format!("{} has no Discord channel yet: place it first", b.to),
        );
    }
    let Ok(Some(t)) = gw.registry.hub_of(&a.id) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub is not running");
    };
    let by = Human {
        id: a.discord_id.clone(),
        name: a.name.clone(),
    };
    let to = b.to.clone();
    let out = t
        .state
        .handle
        .call(
            move |c, now| match c.move_agent(&by, &project, &name, &to, now) {
                Ok((row, fx)) => (Ok(row.name), fx),
                Err(MoveError::Denied(d)) => {
                    (Err((StatusCode::FORBIDDEN, format!("{d:?}"))), vec![])
                }
                Err(MoveError::Refused(why)) => (Err((StatusCode::CONFLICT, why)), vec![]),
            },
        )
        .await;
    match out {
        Some(Ok(name)) => json_reply(StatusCode::OK, json!({"ok": true, "agent": name})),
        Some(Err((code, why))) => err(code, &why),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "your hub is not answering"),
    }
}

#[derive(Deserialize)]
struct SpawnAsk {
    project: String,
    name: String,
    #[serde(default)]
    adapter: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

/// Starts another agent on one of the account's machines, in a folder that machine already knows for the project (the machine decides).
async fn spawn(State(gw): State<Gateway>, headers: HeaderMap, Json(b): Json<SpawnAsk>) -> Response {
    let Some(a) = account_of(&gw, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if !is_slug(&b.project) {
        return err(
            StatusCode::BAD_REQUEST,
            "names use letters, digits, dots, dashes and underscores",
        );
    }
    if let Some(why) = crate::protocol::agent_name_problem(&b.name) {
        return err(StatusCode::BAD_REQUEST, why);
    }
    let b = SpawnAsk {
        name: b.name.to_ascii_lowercase(),
        ..b
    };
    let adapter = match b.adapter.as_deref().unwrap_or("claude") {
        "claude" => AdapterId::Claude,
        "codex" => AdapterId::Codex,
        "agy" => AdapterId::Agy,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "the agent program is claude, codex or agy",
            );
        }
    };
    let Ok(t) = gw.registry.hub(&a) else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "your hub could not start");
    };
    let spec = AgentSpec {
        agent_id: format!("{}/{}", b.project, b.name),
        name: b.name.clone(),
        project: b.project.clone(),
        adapter,
        model: None,
        role: None,
    };
    let by = Human {
        id: a.discord_id.clone(),
        name: a.name.clone(),
    };
    let (project, node, label) = (b.project, b.node, b.label);
    let r = t
        .state
        .handle
        .call(move |c, _| {
            let out = match &node {
                Some(n) => c
                    .spawn(&by, &project, n, spec)
                    .map(|(sent, fx)| (sent.then(|| n.clone()), fx)),
                None => c.spawn_auto(&by, &project, spec, label.as_deref()),
            };
            match out {
                Ok((n, fx)) => (Ok(n), fx),
                Err(d) => (Err(format!("{d:?}")), vec![]),
            }
        })
        .await;
    match r {
        Some(Ok(Some(node))) => json_reply(StatusCode::OK, json!({"node": node})),
        Some(Ok(None)) => err(
            StatusCode::CONFLICT,
            "no connected machine has room (or the right label)",
        ),
        Some(Err(e)) => err(StatusCode::FORBIDDEN, &e),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "your hub is not answering"),
    }
}
