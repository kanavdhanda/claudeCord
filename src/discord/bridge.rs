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

/// How the bridge connects.
#[derive(Clone)]
pub struct BridgeConfig {
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

const CHECK: &str = "\u{2705}";
const REFUSED: &str = "\u{26D4}";

/// Starts the bridge in the background.
pub fn spawn(
    handle: HubHandle,
    chat: broadcast::Receiver<Chat>,
    cfg: BridgeConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(handle, chat, cfg))
}

struct Bridge {
    rest: Rest,
    kv: Store,
    handle: HubHandle,
    guild: String,
    owner: Option<String>,
    /// The last status text shown per project, and when, so the status message is only edited when it changes.
    status_shown: HashMap<String, (String, Instant)>,
}

/// The main loop: chat effects out, Discord events in.
async fn run(handle: HubHandle, mut chat: broadcast::Receiver<Chat>, cfg: BridgeConfig) {
    let Ok(kv) = Store::open(&cfg.db_path, None) else {
        eprintln!(
            "discord bridge: cannot open the database at {}",
            cfg.db_path.display()
        );
        return;
    };
    let rest = Rest::new(&cfg.api_base, &cfg.token);
    let me = rest.me().await.unwrap_or(Value::Null);
    let app_id = me["id"].as_str().unwrap_or("").to_string();
    let owner = me["owner"]["id"]
        .as_str()
        .or_else(|| me["team"]["owner_user_id"].as_str())
        .map(String::from);
    let mut owners = cfg.owners.clone();
    owners.extend(owner.clone());
    for o in owners {
        handle.call(move |c, _| (c.add_owner(&o), vec![])).await;
    }
    if !app_id.is_empty()
        && let Err(e) = rest
            .register_commands(&app_id, &cfg.guild, commands::definitions())
            .await
    {
        eprintln!("discord bridge: could not register slash commands: {e}");
    }
    // Down until the gateway says READY, so a bridge that never connects shows as down.
    let _ = kv.uptime_set("discord", crate::uptime::State::Down, crate::now_ms());
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
        rest,
        kv,
        handle,
        guild: cfg.guild.clone(),
        owner,
        status_shown: HashMap::new(),
    };
    loop {
        tokio::select! {
            c = chat.recv() => match c {
                Ok(c) => b.outward(c).await,
                Err(broadcast::error::RecvError::Lagged(n)) => eprintln!("discord bridge: fell behind and skipped {n} chat event(s)"),
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

    /// The Discord channel for a project, found by name or made, remembered either way.
    async fn channel(&mut self, project: &str) -> Option<String> {
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
        Some(id)
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
        let key = format!("thread:{project}:{name}");
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
            eprintln!("discord bridge: {e}");
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
        if let Some(state) = state {
            let _ = self.kv.uptime_set("discord", state, crate::now_ms());
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
        let reference = format!("{channel}:{mid}");
        let (p2, h2, t2, th2, r2) = (
            project.clone(),
            human.clone(),
            text.clone(),
            thread.clone(),
            reference.clone(),
        );
        let outcome = self
            .handle
            .call(move |c, now| {
                let opts = MessageOpts {
                    thread: th2.as_deref(),
                    reference: Some(&r2),
                    answers_ask: answers.as_deref(),
                    attachments: &[],
                };
                match c.human_message(&h2, &p2, &t2, &opts, now) {
                    Ok((_, fx)) => (Ok(()), fx),
                    Err(d) => (Err(d), vec![]),
                }
            })
            .await;
        match outcome {
            Some(Err(Denied::Unlisted)) | None => return,
            Some(Err(_)) => {
                let _ = self.rest.react(channel, mid, REFUSED).await;
                return;
            }
            Some(Ok(())) => {}
        }
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
            let Ok(data) = self.rest.download(&url).await else {
                continue;
            };
            let (p3, h3, th3, cap) = (project.clone(), human.clone(), thread.clone(), text.clone());
            self.handle
                .call(
                    move |c, _| match c.send_file(&h3, &p3, &cap, &fname, &data, th3, &fid) {
                        Ok((_, fx)) => ((), fx),
                        Err(_) => ((), vec![]),
                    },
                )
                .await;
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
