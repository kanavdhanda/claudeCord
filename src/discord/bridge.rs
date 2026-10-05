//! The bridge between Discord and the hub. It does two jobs and nothing else.
//!
//! Outward: whatever the hub core wants shown in chat (an agent's message, a question with buttons, a permission request,
//! a notice, a file) becomes the right Discord call: a webhook post under the agent's own name, a thread, a message with
//! buttons, an edit, a reaction.
//!
//! Inward: what happens in Discord (a message, a reply to a question, a button press, a slash command) is turned into a call
//! on the core, always as a named person, never as text. Anything from a bot or a webhook is ignored, so an agent's own
//! posts can never come back in as a human. Who may do what is decided by the core, not here.
//!
//! The bridge remembers which channel is which project, which thread is which, and which message holds which question, in
//! the hub's database, so a restart picks up exactly where it was.

use super::api::Rest;
use super::commands;
use super::gateway::{self, Event, GatewayOpts};
use crate::hub::{Answerer, Chat, Decision, Denied, Human, MessageOpts};
use crate::protocol::MAX_FILE_BYTES;
use crate::server::HubHandle;
use crate::store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// Where one project posts, as the hosted service's dashboard set it.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// Which of the account's bots serves this project.
    pub bot: String,
    pub guild: String,
    pub channel: String,
}

/// Where each project of an account lives in Discord. In the hosted service the person picks this on the dashboard, so the bridge asks
/// instead of finding a channel by name in one server.
pub trait Targets: Send + Sync {
    /// Where a project posts, if it has been placed.
    fn target(&self, project: &str) -> Option<(String, Target)>;
    /// Every placed project served by this bot, as (project, target).
    fn mine(&self, bot: &str) -> Vec<(String, Target)>;
}

/// What makes a bridge serve only one bot of an account, in any of that bot's servers.
#[derive(Clone)]
pub struct Scope {
    pub bot_id: String,
    pub targets: std::sync::Arc<dyn Targets>,
}

/// How the bridge connects.
#[derive(Clone)]
pub struct BridgeConfig {
    /// Set in the hosted service: this bridge serves one bot and the places the dashboard chose. None is the single-server mode.
    pub scope: Option<Scope>,
    /// Normally `https://discord.com/api/v10`.
    pub api_base: String,
    pub token: String,
    /// The Discord server (guild) the team lives in.
    pub guild: String,
    /// Replaces the gateway address Discord gives out. Tests use it.
    pub gateway_url: Option<String>,
    /// The hub's database file, where the bridge keeps its small facts.
    pub db_path: PathBuf,
    /// Extra account ids to treat as owners besides the bot's own owner.
    pub owners: Vec<String>,
    pub backoff_max: Duration,
}

/// A Discord message id for a moment in time (ids are the time since 2015 in the high bits), a minute earlier than `now` to allow for the
/// clocks of this machine and Discord not agreeing. Channels are watched from here on, so nothing said before they were known is ever replayed.
fn snowflake_now() -> u64 {
    ((crate::now_ms() - 1_420_070_400_000 - 60_000).max(0) as u64) << 22
}

/// Starts watching a channel: its messages are taken from now on, once each, and read back after an outage.
async fn watch(handle: &HubHandle, channel: &str) {
    let (c, from) = (channel.to_string(), snowflake_now());
    handle
        .call(move |core, _| (core.watch_chat(&c, from), vec![]))
        .await;
}

const CHECK: &str = "\u{2705}";
const REFUSED: &str = "\u{26D4}";

/// Starts the bridge in the background, supervised: if it panics it is logged and started again, so Discord never quietly stops
/// while the rest of the hub carries on.
pub fn spawn(
    handle: HubHandle,
    chat: broadcast::Receiver<Chat>,
    cfg: BridgeConfig,
) -> tokio::task::JoinHandle<()> {
    crate::task::supervised("discord bridge", move || {
        run(handle.clone(), chat.resubscribe(), cfg.clone())
    })
}

struct Bridge {
    scope: Option<Scope>,
    /// Channels and threads this bridge serves (only used with a scope), so it never reads another bot's channels.
    owned: std::collections::HashSet<String>,
    rest: Rest,
    kv: Store,
    handle: HubHandle,
    guild: String,
    /// The name this bridge's up/down record is kept under.
    component: String,
    owner: Option<String>,
    /// The last status text shown per project, and when, so the status message is only edited when it changes.
    status_shown: HashMap<String, (String, Instant)>,
}

/// The main loop: chat effects out, Discord events in.
async fn run(handle: HubHandle, mut chat: broadcast::Receiver<Chat>, cfg: BridgeConfig) {
    let Ok(kv) = Store::open(&cfg.db_path, None) else {
        crate::error!(
            "discord",
            "cannot open the database at {}",
            cfg.db_path.display()
        );
        return;
    };
    let rest = Rest::new(&cfg.api_base, &cfg.token);
    // Asks Discord who the bot is. If Discord cannot be reached, or refuses the token, say so and keep trying, so a Discord outage at
    // start-up only delays the bridge instead of leaving it half set up for good.
    let me = loop {
        match rest.me().await {
            Ok(v) => break v,
            Err(e) => {
                crate::error!(
                    "discord",
                    "cannot reach Discord, or it refused the bot token ({e}); trying again soon"
                );
                tokio::time::sleep(cfg.backoff_max.min(Duration::from_secs(30))).await;
            }
        }
    };
    let app_id = me["id"].as_str().unwrap_or("").to_string();
    let owner = me["owner"]["id"]
        .as_str()
        .or_else(|| me["team"]["owner_user_id"].as_str())
        .map(String::from);
    let mut owners = cfg.owners.clone();
    // In the hosted service the owner is the account's own person; a bot's Discord owner gets no say in someone else's team.
    let owner = if cfg.scope.is_some() {
        cfg.owners.first().cloned()
    } else {
        owners.extend(owner.clone());
        owner
    };
    for o in owners {
        handle.call(move |c, _| (c.add_owner(&o), vec![])).await;
    }
    if !app_id.is_empty() {
        // One server in the single-server mode; every server the bot is in when it serves an account.
        let guilds: Vec<String> = if cfg.scope.is_some() {
            rest.my_guilds()
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|g| g["id"].as_str().map(String::from))
                .collect()
        } else {
            vec![cfg.guild.clone()]
        };
        for g in guilds {
            if let Err(e) = rest
                .register_commands(&app_id, &g, commands::definitions())
                .await
            {
                crate::error!(
                    "discord",
                    "could not register the slash commands in {g}: {e}"
                );
            }
        }
    }
    // Down until the gateway says READY, so a bridge that never connects shows as down.
    let component = match &cfg.scope {
        Some(sc) => format!("discord:{}", sc.bot_id),
        None => "discord".to_string(),
    };
    let _ = kv.uptime_set(&component, crate::uptime::State::Down, crate::now_ms());
    let (ev_tx, mut events) = mpsc::channel(256);
    let _gateway = gateway::spawn(
        rest.clone(),
        GatewayOpts {
            token: cfg.token.clone(),
            url: cfg.gateway_url.clone(),
            backoff_max: cfg.backoff_max,
        },
        ev_tx,
    );
    let mut b = Bridge {
        scope: cfg.scope.clone(),
        owned: Default::default(),
        component,
        rest,
        kv,
        handle,
        guild: cfg.guild.clone(),
        owner,
        status_shown: HashMap::new(),
    };
    b.watch_known().await;
    loop {
        tokio::select! {
            c = chat.recv() => match c {
                Ok(c) => b.outward(c).await,
                Err(broadcast::error::RecvError::Lagged(n)) => crate::warn!("discord", "fell behind and skipped {n} chat event(s)"),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            e = events.recv() => match e {
                Some(e) => b.inward(e).await,
                None => return,
            },
        }
    }
}

impl Bridge {
    fn get(&self, key: &str) -> Option<String> {
        self.kv.kv_get(key).ok().flatten()
    }

    fn set(&self, key: &str, value: &str) {
        let _ = self.kv.kv_set(key, value);
    }

    /// Watches every channel and thread this bridge already knew from earlier runs (those not yet watched start from now).
    async fn watch_known(&mut self) {
        if let Some(sc) = self.scope.clone() {
            // Serving an account: what this bridge knows is what the dashboard placed for its bot, plus the threads made in those places.
            for (project, t) in sc.targets.mine(&sc.bot_id) {
                self.adopt(&project, &t.channel).await;
            }
            for (k, v) in self.kv.kv_scan("threadrev:").unwrap_or_default() {
                let id = k["threadrev:".len()..].to_string();
                let project = v.split('\n').next().unwrap_or("");
                if sc
                    .targets
                    .target(project)
                    .is_some_and(|(_, t)| t.bot == sc.bot_id)
                {
                    self.owned.insert(id.clone());
                    watch(&self.handle, &id).await;
                }
            }
            return;
        }
        let known = ["chanrev:", "threadrev:"]
            .iter()
            .flat_map(|prefix| {
                self.kv
                    .kv_scan(prefix)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |(k, _)| k[prefix.len()..].to_string())
            })
            .collect::<Vec<_>>();
        for channel in known {
            watch(&self.handle, &channel).await;
        }
    }

    /// After a fresh connection to Discord, reads what was said in every watched channel since the last message taken, oldest first, and
    /// handles each as if it had just arrived. Messages already taken are skipped (the hub remembers the newest per channel, saved with the
    /// message itself), so this is safe to run any time. It runs before live events are handled, so order is kept.
    ///
    /// ponytail: channels are read one after another, so with thousands of them a start-up takes as long as Discord's rate limit allows
    /// (about 50 calls a second); read busy channels first, or in the background, if that ever matters.
    async fn backfill(&mut self) {
        let channels = self
            .handle
            .read(|c, _| c.watched_chats())
            .await
            .unwrap_or_default();
        let mut taken = 0usize;
        for (channel, last) in channels {
            // Another bot of the same account serves the channels that are not this bridge's.
            if self.scope.is_some() && !self.owned.contains(&channel) {
                continue;
            }
            let mut after = last;
            loop {
                let mut msgs = match self.rest.messages_after(&channel, &after.to_string()).await {
                    Ok(m) => m,
                    Err(e) => {
                        crate::warn!(
                            "discord",
                            "could not read back the messages of channel {channel}: {e}"
                        );
                        break;
                    }
                };
                let id_of = |m: &Value| {
                    m["id"]
                        .as_str()
                        .and_then(|i| i.parse::<u64>().ok())
                        .unwrap_or(0)
                };
                msgs.sort_by_key(id_of);
                let n = msgs.len();
                for mut m in msgs {
                    if m["channel_id"].is_null() {
                        m["channel_id"] = Value::String(channel.clone());
                    }
                    after = after.max(id_of(&m));
                    self.on_message(&m).await;
                    taken += 1;
                }
                if n < 100 {
                    break;
                }
            }
        }
        if taken > 0 {
            crate::info!(
                "discord",
                "read back {taken} message(s) sent while the bridge was not connected"
            );
        }
    }

    /// The Discord channel for a project, found by name or made, remembered either way.
    async fn channel(&mut self, project: &str) -> Option<String> {
        if let Some(sc) = self.scope.clone() {
            // Serving an account: the place is the one the person chose on the dashboard, never a guess by name. A project nobody has
            // placed, or one placed with another of the account's bots, is not this bridge's to post.
            let (_, t) = sc.targets.target(project)?;
            if t.bot != sc.bot_id {
                return None;
            }
            if !self.owned.contains(&t.channel) {
                self.adopt(project, &t.channel).await;
            }
            return Some(t.channel);
        }
        if let Some(id) = self.get(&format!("chan:{project}")) {
            return Some(id);
        }
        let existing = self.rest.guild_channels(&self.guild).await.ok()?;
        let id = match existing
            .iter()
            .find(|c| c["name"] == project && c["type"] == 0)
            .and_then(|c| c["id"].as_str())
        {
            Some(id) => id.to_string(),
            None => self
                .rest
                .create_channel(&self.guild, project, "claudeCord project")
                .await
                .ok()?,
        };
        self.set(&format!("chan:{project}"), &id);
        self.set(&format!("chanrev:{id}"), project);
        watch(&self.handle, &id).await;
        Some(id)
    }

    /// Starts serving a channel for a project: remembers which project it is, and starts taking what people say in it.
    async fn adopt(&mut self, project: &str, channel: &str) {
        self.set(&format!("chan:{project}"), channel);
        self.set(&format!("chanrev:{channel}"), project);
        self.owned.insert(channel.to_string());
        watch(&self.handle, channel).await;
    }

    /// The webhook for a channel (made once), as (id, token).
    async fn webhook(&mut self, channel: &str) -> Option<(String, String)> {
        if let Some(v) = self.get(&format!("wh:{channel}"))
            && let Some((i, t)) = v.split_once(':')
        {
            return Some((i.into(), t.into()));
        }
        let (i, t) = self.rest.create_webhook(channel, "claudeCord").await.ok()?;
        self.set(&format!("wh:{channel}"), &format!("{i}:{t}"));
        Some((i, t))
    }

    /// The Discord thread for a named thread in a project (made on first use).
    async fn thread(&mut self, project: &str, channel: &str, name: &str) -> Option<String> {
        // Serving an account, the key names the channel too: a project moved to another channel must not reuse the old channel's thread.
        let key = if self.scope.is_some() {
            format!("thread:{project}:{channel}:{name}")
        } else {
            format!("thread:{project}:{name}")
        };
        if let Some(id) = self.get(&key) {
            return Some(id);
        }
        let id = self
            .rest
            .create_thread(channel, &name.chars().take(100).collect::<String>())
            .await
            .ok()?;
        self.set(&key, &id);
        self.set(&format!("threadrev:{id}"), &format!("{project}\n{name}"));
        self.owned.insert(id.clone());
        watch(&self.handle, &id).await;
        Some(id)
    }

    /// Where to post for a project and optional thread: (parent channel, thread id if any).
    async fn place(
        &mut self,
        project: &str,
        thread: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        let channel = self.channel(project).await?;
        let thread_id = match thread {
            Some(t) if !t.is_empty() => self.thread(project, &channel, t).await,
            _ => None,
        };
        Some((channel, thread_id))
    }

    /// Carries out one thing the core wants shown. Failures are reported to the log and never stop the bridge.
    async fn outward(&mut self, c: Chat) {
        // Serving an account: a project placed with another of its bots, or not placed at all, is not this bridge's to show.
        if let Some(sc) = &self.scope
            && !sc
                .targets
                .target(c.project())
                .is_some_and(|(_, t)| t.bot == sc.bot_id)
        {
            return;
        }
        let result: Result<(), String> = async {
            match c {
                Chat::EnsureProject(p) => {
                    self.channel(&p).await.ok_or("could not find or make the channel")?;
                }
                Chat::Post { project, agent, text, thread } => {
                    let (ch, th) = self.place(&project, thread.as_deref()).await.ok_or("no channel")?;
                    let (id, tok) = self.webhook(&ch).await.ok_or("no webhook")?;
                    self.rest.webhook_send(&id, &tok, &agent.name, &text, th.as_deref()).await.map_err(|e| e.to_string())?;
                }
                Chat::Report { project, agent, title, summary, artifacts } => {
                    let (ch, _) = self.place(&project, None).await.ok_or("no channel")?;
                    let (id, tok) = self.webhook(&ch).await.ok_or("no webhook")?;
                    let list = artifacts.map(|a| format!("\n{}", a.join("\n"))).unwrap_or_default();
                    self.rest.webhook_send(&id, &tok, &agent.name, &format!("**{title}**\n{summary}{list}"), None).await.map_err(|e| e.to_string())?;
                }
                Chat::File { project, agent, name, data, caption, thread } => {
                    let (ch, th) = self.place(&project, thread.as_deref()).await.ok_or("no channel")?;
                    let (id, tok) = self.webhook(&ch).await.ok_or("no webhook")?;
                    self.rest.webhook_file(&id, &tok, &agent.name, &name, data, caption.as_deref().unwrap_or(""), th.as_deref()).await.map_err(|e| e.to_string())?;
                }
                Chat::Ask { project, agent, ask } => {
                    let (ch, th) = self.place(&project, ask.thread.as_deref()).await.ok_or("no channel")?;
                    let target = th.clone().unwrap_or(ch);
                    let label = format!("Q{}", ask.qn);
                    let buttons = ask.options.as_ref().map(|opts| {
                        let row: Vec<Value> = opts.iter().take(5).enumerate().map(|(i, o)| json!({"type": 2, "style": 1, "label": o.chars().take(80).collect::<String>(), "custom_id": format!("ask:{project}:{label}:{i}")})).collect();
                        json!([{"type": 1, "components": row}])
                    });
                    let text = format!("**{} asks ({label})**\n{}\nReply to this message to answer.", agent.name, ask.question);
                    let mid = self.rest.send(&target, &text, buttons, &[]).await.map_err(|e| e.to_string())?;
                    self.set(&format!("msg:{project}:{label}"), &format!("{target}:{mid}"));
                    self.set(&format!("askmsg:{mid}"), &format!("{project}:{label}"));
                }
                Chat::Permission { project, agent, perm } => {
                    let (ch, th) = self.place(&project, perm.thread.as_deref()).await.ok_or("no channel")?;
                    let target = th.unwrap_or(ch);
                    let label = format!("P{}", perm.pn);
                    let b = |text: &str, style: u8, what: &str| json!({"type": 2, "style": style, "label": text, "custom_id": format!("perm:{project}:{label}:{what}")});
                    let row = json!([{"type": 1, "components": [b("Deny", 4, "deny"), b("Allow once", 3, "once"), b("Allow this kind", 1, "kind"), b("Allow all", 2, "all")]}]);
                    let risk = if perm.risk == crate::hub::Risk::High { "HIGH RISK, owner only" } else { "normal" };
                    let text = format!("**{} wants permission ({label}, {risk})**\n{}: {}", agent.name, perm.kind, perm.action.chars().take(1500).collect::<String>());
                    let mention: Vec<String> = if perm.risk == crate::hub::Risk::High { self.owner.clone().into_iter().collect() } else { vec![] };
                    let text = match mention.first() {
                        Some(o) => format!("<@{o}> {text}"),
                        None => text,
                    };
                    let mid = self.rest.send(&target, &text, Some(row), &mention).await.map_err(|e| e.to_string())?;
                    self.set(&format!("msg:{project}:{label}"), &format!("{target}:{mid}"));
                }
                Chat::Resolved { project, label, how } => {
                    if let Some(v) = self.get(&format!("msg:{project}:{label}"))
                        && let Some((ch, mid)) = v.split_once(':')
                    {
                        self.rest.edit(ch, mid, &format!("{label}: {how}"), json!([])).await.map_err(|e| e.to_string())?;
                    }
                }
                Chat::Notice { project, text, mention } => {
                    let (ch, _) = self.place(&project, None).await.ok_or("no channel")?;
                    let users: Vec<String> = if mention { self.owner.clone().into_iter().collect() } else { vec![] };
                    let text = match users.first() {
                        Some(o) => format!("<@{o}> {text}"),
                        None => text,
                    };
                    self.rest.send(&ch, &text, None, &users).await.map_err(|e| e.to_string())?;
                }
                Chat::Confirm { reference, .. } => {
                    if let Some((ch, mid)) = reference.split_once(':') {
                        self.rest.react(ch, mid, CHECK).await.map_err(|e| e.to_string())?;
                    }
                }
                Chat::RefreshStatus(project) => self.refresh_status(&project).await?,
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            crate::error!("discord", "{e}");
        }
    }

    /// Keeps one status message per project up to date: who is here and what each is doing. Edited at most every few
    /// seconds and only when the text changed, so a flurry of status changes is one edit.
    async fn refresh_status(&mut self, project: &str) -> Result<(), String> {
        let p = project.to_string();
        let lines: Vec<String> = self
            .handle
            .call(move |c, _| {
                let rows = c
                    .agents_of_project(&p)
                    .iter()
                    .map(|a| {
                        format!(
                            "{}{}: {:?}",
                            a.name,
                            if a.is_lead { " (lead)" } else { "" },
                            c.status_of(&a.agent_id)
                        )
                    })
                    .collect();
                (rows, vec![])
            })
            .await
            .unwrap_or_default();
        let text = format!("**Team**\n{}", lines.join("\n"));
        if self
            .status_shown
            .get(project)
            .is_some_and(|(t, at)| *t == text || at.elapsed() < Duration::from_secs(3))
        {
            return Ok(());
        }
        let ch = self.channel(project).await.ok_or("no channel")?;
        match self.get(&format!("status:{project}")) {
            Some(v) if v.split_once(':').is_some() => {
                let (c, m) = v.split_once(':').expect("checked");
                self.rest
                    .edit(c, m, &text, json!([]))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            _ => {
                let mid = self
                    .rest
                    .send(&ch, &text, None, &[])
                    .await
                    .map_err(|e| e.to_string())?;
                self.set(&format!("status:{project}"), &format!("{ch}:{mid}"));
            }
        }
        self.status_shown
            .insert(project.to_string(), (text, Instant::now()));
        Ok(())
    }

    /// Which project (and thread name) a Discord channel belongs to.
    fn project_of(&self, channel: &str) -> Option<(String, Option<String>)> {
        if let Some(p) = self.get(&format!("chanrev:{channel}")) {
            return Some((p, None));
        }
        let v = self.get(&format!("threadrev:{channel}"))?;
        let (p, t) = v.split_once('\n')?;
        Some((p.to_string(), Some(t.to_string())))
    }

    /// Handles one event from Discord.
    async fn inward(&mut self, e: Event) {
        // The gateway being connected is what "the Discord bridge is up" means; see `crate::uptime`.
        let state = match e.name.as_str() {
            "READY" | "RESUMED" => Some(crate::uptime::State::Up),
            gateway::DISCONNECTED => Some(crate::uptime::State::Down),
            _ => None,
        };
        if let Some(state) = state
            && self
                .kv
                .uptime_set(&self.component, state, crate::now_ms())
                .unwrap_or(false)
        {
            match state {
                crate::uptime::State::Up => {
                    crate::info!("discord", "connected to the Discord gateway")
                }
                crate::uptime::State::Down => crate::warn!(
                    "discord",
                    "lost the Discord gateway connection; reconnecting"
                ),
            }
        }
        // A fresh session (not a resume) may have missed messages: read back what was said while away, before live events go on.
        if e.name == "READY" {
            self.backfill().await;
        }
        match e.name.as_str() {
            "MESSAGE_CREATE" => self.on_message(&e.data).await,
            "INTERACTION_CREATE" => self.on_interaction(&e.data).await,
            _ => {}
        }
    }

    /// A message in a channel. Bots and webhooks are ignored. Anything else becomes a message from a named person, a reply
    /// to a question (if it replies to one), and any attachments are passed on as files.
    async fn on_message(&mut self, m: &Value) {
        if m["author"]["bot"].as_bool().unwrap_or(false) || !m["webhook_id"].is_null() {
            return;
        }
        let (Some(channel), Some(mid), Some(uid)) = (
            m["channel_id"].as_str(),
            m["id"].as_str(),
            m["author"]["id"].as_str(),
        ) else {
            return;
        };
        let Some((project, thread)) = self.project_of(channel) else {
            return;
        };
        let name = m["member"]["nick"]
            .as_str()
            .or_else(|| m["author"]["global_name"].as_str())
            .or_else(|| m["author"]["username"].as_str())
            .unwrap_or("someone")
            .to_string();
        let human = Human {
            id: uid.to_string(),
            name,
        };
        let text = m["content"].as_str().unwrap_or("").to_string();
        let answers = m["message_reference"]["message_id"]
            .as_str()
            .and_then(|r| self.get(&format!("askmsg:{r}")))
            .and_then(|v| v.strip_prefix(&format!("{project}:")).map(String::from));
        let files: Vec<(String, String, usize, String)> = m["attachments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some((
                            x["id"].as_str()?.to_string(),
                            x["filename"].as_str()?.to_string(),
                            x["size"].as_u64()? as usize,
                            x["url"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        // Someone who is not on the project's list is ignored before anything is downloaded for them.
        let (pp, uu) = (project.clone(), uid.to_string());
        if !self
            .handle
            .read(move |c, _| c.role_of(&pp, &uu).is_some())
            .await
            .unwrap_or(false)
        {
            return;
        }
        // The attachments are fetched first, so that the message and its files are taken in ONE step below: either all of it is saved, or
        // none of it is, and a retry after a crash cannot take half of it twice.
        let mut fetched: Vec<(String, String, Vec<u8>)> = Vec::new();
        for (fid, fname, size, url) in files {
            if size > MAX_FILE_BYTES {
                let _ = self
                    .rest
                    .send(
                        channel,
                        &format!(
                            "{fname} is over the {} MB limit, so it was not passed on.",
                            MAX_FILE_BYTES / 1_048_576
                        ),
                        None,
                        &[],
                    )
                    .await;
                continue;
            }
            if let Ok(data) = self.rest.download(&url).await {
                fetched.push((fid, fname, data));
            }
        }
        let reference = format!("{channel}:{mid}");
        let (p2, h2, t2, th2, r2, chan) = (
            project.clone(),
            human.clone(),
            text.clone(),
            thread.clone(),
            reference.clone(),
            channel.to_string(),
        );
        let mid_num: Option<u64> = mid.parse().ok();
        let outcome = self
            .handle
            .call(move |c, now| {
                // A message already taken (read back after an outage, or replayed by a resumed connection) is not taken again. The record
                // of it is saved in the same commit as what the message causes, so a crash cannot separate the two.
                if let Some(n) = mid_num
                    && !c.take_chat_message(&chan, n)
                {
                    return (Ok(()), vec![]);
                }
                let opts = MessageOpts {
                    thread: th2.as_deref(),
                    reference: Some(&r2),
                    answers_ask: answers.as_deref(),
                    attachments: &[],
                };
                let mut fx = match c.human_message(&h2, &p2, &t2, &opts, now) {
                    Ok((_, fx)) => fx,
                    Err(d) => return (Err(d), vec![]),
                };
                for (fid, fname, data) in &fetched {
                    if let Ok((_, f)) = c.send_file(&h2, &p2, &t2, fname, data, th2.clone(), fid) {
                        fx.extend(f);
                    }
                }
                (Ok(()), fx)
            })
            .await;
        match outcome {
            Some(Err(Denied::Unlisted)) | None => {}
            Some(Err(_)) => {
                let _ = self.rest.react(channel, mid, REFUSED).await;
            }
            Some(Ok(())) => {}
        }
    }

    /// A button press (type 3) or a slash command (type 2).
    async fn on_interaction(&mut self, i: &Value) {
        let (Some(iid), Some(token), Some(channel)) = (
            i["id"].as_str(),
            i["token"].as_str(),
            i["channel_id"].as_str(),
        ) else {
            return;
        };
        let user = &i["member"]["user"];
        let Some(uid) = user["id"].as_str() else {
            return;
        };
        let Some((project, _)) = self.project_of(channel) else {
            let _ = self
                .rest
                .respond(
                    iid,
                    token,
                    "This channel is not a claudeCord project.",
                    true,
                )
                .await;
            return;
        };
        let name = i["member"]["nick"]
            .as_str()
            .or_else(|| user["global_name"].as_str())
            .or_else(|| user["username"].as_str())
            .unwrap_or("someone")
            .to_string();
        let human = Human {
            id: uid.to_string(),
            name,
        };
        let reply: String = match i["type"].as_u64() {
            Some(3) => {
                let id = i["data"]["custom_id"].as_str().unwrap_or("").to_string();
                self.on_button(&id, &human, &project).await
            }
            Some(2) => {
                let cmd = i["data"]["name"].as_str().unwrap_or("").to_string();
                let mut opts: commands::Opts = HashMap::new();
                for o in i["data"]["options"].as_array().cloned().unwrap_or_default() {
                    let (Some(k), v) = (o["name"].as_str(), &o["value"]) else {
                        continue;
                    };
                    let val = v
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string());
                    if o["type"] == 6 {
                        let resolved = &i["data"]["resolved"];
                        let n = resolved["members"][&val]["nick"]
                            .as_str()
                            .or_else(|| resolved["users"][&val]["username"].as_str())
                            .unwrap_or("someone");
                        opts.insert(format!("{k}_name"), n.to_string());
                    }
                    opts.insert(k.to_string(), val);
                }
                let h = human.clone();
                let p = project.clone();
                self.handle
                    .call(move |c, now| commands::handle(c, &cmd, &opts, &h, &p, now))
                    .await
                    .unwrap_or_else(|| "The hub is not answering.".into())
            }
            _ => return,
        };
        let _ = self.rest.respond(iid, token, &reply, true).await;
    }

    /// A button on a question or a permission request.
    async fn on_button(&mut self, custom_id: &str, human: &Human, project: &str) -> String {
        let parts: Vec<&str> = custom_id.split(':').collect();
        let (h, p) = (human.clone(), project.to_string());
        match parts.as_slice() {
            ["ask", _, label, idx] => {
                let (label, idx) = (
                    label.to_string(),
                    idx.parse::<usize>().unwrap_or(usize::MAX),
                );
                self.handle
                    .call(move |c, now| {
                        let option = c
                            .asks_of(&p)
                            .iter()
                            .find(|a| format!("Q{}", a.qn) == label)
                            .and_then(|a| a.options.as_ref()?.get(idx).cloned());
                        let Some(text) = option else {
                            return ("That choice no longer exists.".to_string(), vec![]);
                        };
                        match c.answer_ask(&Answerer::Human(h), &p, &label, &text, now) {
                            Ok(o) => (format!("Answered: {text}"), o.effects),
                            Err(d) => (commands::denied_text(&d), vec![]),
                        }
                    })
                    .await
                    .unwrap_or_default()
            }
            ["perm", _, label, what] => {
                let (label, what) = (label.to_string(), what.to_string());
                let decision = match what.as_str() {
                    "deny" => Decision::Deny,
                    "once" => Decision::Once,
                    "kind" => Decision::Kind,
                    _ => Decision::All,
                };
                self.handle
                    .call(move |c, now| {
                        match c.decide_permission(&h, &p, &label, decision, None, now) {
                            Ok(fx) => (format!("Done ({what})."), fx),
                            Err(d) => (commands::denied_text(&d), vec![]),
                        }
                    })
                    .await
                    .unwrap_or_default()
            }
            _ => "Unknown button.".into(),
        }
    }
}
