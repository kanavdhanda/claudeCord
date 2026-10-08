//! A small client for the parts of Discord's REST API the bridge uses: finding and making channels, webhooks (so each
//! agent posts under its own name), threads, messages with buttons, reactions, slash commands and attachments.
//!
//! Every call goes through `call`, which waits and retries when Discord says to slow down (a 429 answer) instead of
//! failing. The base address is a parameter, so tests point it at a stand-in server.

use reqwest::{Client, Method, multipart};
use serde_json::{Value, json};
use std::time::Duration;

/// Discord's REST client for one bot.
#[derive(Clone)]
pub struct Rest {
    http: Client,
    base: String,
    token: String,
}

/// Why a call failed: the answer's status and body, or a network problem.
#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, ApiError>;

/// How many times a rate-limited call is retried before giving up.
const MAX_RETRIES: u32 = 4;

impl Rest {
    /// A client for the bot with this token. `base` is normally `https://discord.com/api/v10`.
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// One call. Retries after the wait Discord asks for when it answers 429. Returns the JSON answer (Null if empty).
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut tries = 0;
        loop {
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .header("authorization", format!("Bot {}", self.token));
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| ApiError(e.to_string()))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 && tries < MAX_RETRIES {
                tries += 1;
                let wait = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["retry_after"].as_f64())
                    .unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.0, 30.0))).await;
                continue;
            }
            if !status.is_success() {
                return Err(ApiError(format!(
                    "{status}: {}",
                    text.chars().take(300).collect::<String>()
                )));
            }
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
    }

    /// The bot's own application, including who owns it. The owner becomes the first owner in the hub.
    pub async fn me(&self) -> Result<Value> {
        self.call(Method::GET, "/oauth2/applications/@me", None)
            .await
    }

    /// Who the bot is as a user (its id and name). This is also how a pasted token is proven real: Discord answers 401 for a wrong one.
    pub async fn bot_user(&self) -> Result<Value> {
        self.call(Method::GET, "/users/@me", None).await
    }

    /// The servers the bot has been added to.
    pub async fn my_guilds(&self) -> Result<Vec<Value>> {
        Ok(self
            .call(Method::GET, "/users/@me/guilds", None)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// The address of the real-time gateway.
    pub async fn gateway_url(&self) -> Result<String> {
        let v = self.call(Method::GET, "/gateway/bot", None).await?;
        Ok(v["url"]
            .as_str()
            .unwrap_or("wss://gateway.discord.gg")
            .to_string())
    }

    /// The channels of a server.
    pub async fn guild_channels(&self, guild: &str) -> Result<Vec<Value>> {
        Ok(self
            .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// One channel, as Discord describes it (its topic, among other things).
    pub async fn channel(&self, id: &str) -> Result<Value> {
        self.call(Method::GET, &format!("/channels/{id}"), None)
            .await
    }

    /// Changes a channel's topic.
    pub async fn set_channel_topic(&self, id: &str, topic: &str) -> Result<()> {
        self.call(
            Method::PATCH,
            &format!("/channels/{id}"),
            Some(json!({"topic": topic})),
        )
        .await?;
        Ok(())
    }

    /// The roles of a server.
    pub async fn guild_roles(&self, guild: &str) -> Result<Vec<Value>> {
        Ok(self
            .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// Makes a role anyone can mention, with no permissions of its own, and returns its id. (Discord only offers members, roles and channels
    /// when someone types `@`, and agents post as webhooks, so a role named after each agent is how `@name` shows up in that list.)
    pub async fn create_role(&self, guild: &str, name: &str) -> Result<String> {
        let v = self
            .call(
                Method::POST,
                &format!("/guilds/{guild}/roles"),
                Some(json!({"name": name, "mentionable": true, "permissions": "0"})),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no role id in the answer".into()))
    }

    /// Deletes a role.
    pub async fn delete_role(&self, guild: &str, role: &str) -> Result<()> {
        self.call(
            Method::DELETE,
            &format!("/guilds/{guild}/roles/{role}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Makes a text channel and returns its id.
    pub async fn create_channel(&self, guild: &str, name: &str, topic: &str) -> Result<String> {
        let v = self
            .call(
                Method::POST,
                &format!("/guilds/{guild}/channels"),
                Some(json!({"name": name, "type": 0, "topic": topic})),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no channel id in the answer".into()))
    }

    /// Makes a webhook in a channel and returns (id, token).
    pub async fn create_webhook(&self, channel: &str, name: &str) -> Result<(String, String)> {
        let v = self
            .call(
                Method::POST,
                &format!("/channels/{channel}/webhooks"),
                Some(json!({"name": name})),
            )
            .await?;
        match (v["id"].as_str(), v["token"].as_str()) {
            (Some(i), Some(t)) => Ok((i.into(), t.into())),
            _ => Err(ApiError("no webhook id or token in the answer".into())),
        }
    }

    /// Posts as `username` through a webhook (optionally into a thread). Text over Discord's limit is cut and the full
    /// text is attached as a file. Returns the message id.
    pub async fn webhook_send(
        &self,
        id: &str,
        token: &str,
        username: &str,
        content: &str,
        thread: Option<&str>,
    ) -> Result<String> {
        self.webhook_send_to(id, token, username, content, thread, (&[], &[]))
            .await
    }

    /// Like `webhook_send`, and the people named by `notify` (Discord user ids that appear as `<@id>` in the text) really are notified. Nobody else is.
    pub async fn webhook_send_to(
        &self,
        id: &str,
        token: &str,
        username: &str,
        content: &str,
        thread: Option<&str>,
        (notify, roles): (&[String], &[String]),
    ) -> Result<String> {
        let allowed = json!({"parse": [], "users": notify, "roles": roles});
        let mut path = format!("/webhooks/{id}/{token}?wait=true");
        if let Some(t) = thread {
            path.push_str(&format!("&thread_id={t}"));
        }
        let (short, long) = split_for_discord(content);
        let v = match long {
            None => self.call(Method::POST, &path, Some(json!({"username": username, "content": short, "allowed_mentions": allowed}))).await?,
            Some(full) => {
                let form = multipart::Form::new()
                    .text("payload_json", json!({"username": username, "content": short, "allowed_mentions": allowed}).to_string())
                    .part("files[0]", multipart::Part::bytes(full.clone().into_bytes()).file_name(if looks_like_markdown(&full) { "message.md" } else { "message.txt" }));
                self.multipart(&path, form).await?
            }
        };
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no message id in the answer".into()))
    }

    #[allow(clippy::too_many_arguments)]
    /// Posts a file as `username` through a webhook. Returns the message id.
    pub async fn webhook_file(
        &self,
        id: &str,
        token: &str,
        username: &str,
        name: &str,
        data: Vec<u8>,
        caption: &str,
        thread: Option<&str>,
    ) -> Result<String> {
        let mut path = format!("/webhooks/{id}/{token}?wait=true");
        if let Some(t) = thread {
            path.push_str(&format!("&thread_id={t}"));
        }
        let form = multipart::Form::new()
            .text("payload_json", json!({"username": username, "content": caption.chars().take(1900).collect::<String>(), "allowed_mentions": {"parse": []}}).to_string())
            .part("files[0]", multipart::Part::bytes(data).file_name(name.to_string()));
        let v = self.multipart(&path, form).await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no message id in the answer".into()))
    }

    /// A multipart POST (a message with a file). Not retried on rate limits, since the form is consumed by sending.
    async fn multipart(&self, path: &str, form: multipart::Form) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bot {}", self.token))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ApiError(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError(format!(
                "{status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Starts a public thread in a channel and returns its id.
    pub async fn create_thread(&self, channel: &str, name: &str) -> Result<String> {
        let v = self
            .call(
                Method::POST,
                &format!("/channels/{channel}/threads"),
                Some(json!({"name": name, "type": 11, "auto_archive_duration": 10080})),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no thread id in the answer".into()))
    }

    /// Sends a message as the bot (with optional buttons) and returns its id. Mentions are limited to those listed.
    pub async fn send(
        &self,
        channel: &str,
        content: &str,
        components: Option<Value>,
        mention_users: &[String],
    ) -> Result<String> {
        let mut body = json!({"content": content.chars().take(1990).collect::<String>(), "allowed_mentions": {"parse": [], "users": mention_users}});
        if let Some(c) = components {
            body["components"] = c;
        }
        let v = self
            .call(
                Method::POST,
                &format!("/channels/{channel}/messages"),
                Some(body),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ApiError("no message id in the answer".into()))
    }

    /// Changes a message's text and buttons (an empty list removes the buttons).
    pub async fn edit(
        &self,
        channel: &str,
        message: &str,
        content: &str,
        components: Value,
    ) -> Result<()> {
        self.call(Method::PATCH, &format!("/channels/{channel}/messages/{message}"), Some(json!({"content": content.chars().take(1990).collect::<String>(), "components": components}))).await.map(|_| ())
    }

    /// Adds a reaction to a message. `emoji` is the character itself.
    /// Takes the bot's own reaction off a message again.
    pub async fn unreact(&self, channel: &str, message: &str, emoji: &str) -> Result<()> {
        let enc: String = emoji.bytes().map(|b| format!("%{b:02X}")).collect();
        self.call(
            Method::DELETE,
            &format!("/channels/{channel}/messages/{message}/reactions/{enc}/@me"),
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn react(&self, channel: &str, message: &str, emoji: &str) -> Result<()> {
        let enc: String = emoji.bytes().map(|b| format!("%{b:02X}")).collect();
        self.call(
            Method::PUT,
            &format!("/channels/{channel}/messages/{message}/reactions/{enc}/@me"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Answers a button press or slash command. `ephemeral` shows the answer only to the person who acted.
    pub async fn respond(
        &self,
        interaction: &str,
        token: &str,
        content: &str,
        ephemeral: bool,
    ) -> Result<()> {
        let flags = if ephemeral { 64 } else { 0 };
        self.call(Method::POST, &format!("/interactions/{interaction}/{token}/callback"), Some(json!({"type": 4, "data": {"content": content.chars().take(1990).collect::<String>(), "flags": flags, "allowed_mentions": {"parse": []}}}))).await.map(|_| ())
    }

    /// Answers a slash command without leaving a message: it is acknowledged (so Discord is satisfied) and the acknowledgement is taken away
    /// again. For a command whose real answer arrives separately, such as a picture.
    pub async fn respond_silently(
        &self,
        interaction: &str,
        token: &str,
        app_id: &str,
    ) -> Result<()> {
        self.call(
            Method::POST,
            &format!("/interactions/{interaction}/{token}/callback"),
            Some(json!({"type": 5, "data": {"flags": 64}})),
        )
        .await?;
        self.call(
            Method::DELETE,
            &format!("/webhooks/{app_id}/{token}/messages/@original"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Answers a person who is typing an option with the choices to list (Discord shows at most 25).
    pub async fn choices(&self, interaction: &str, token: &str, names: &[String]) -> Result<()> {
        let list: Vec<Value> = names
            .iter()
            .take(25)
            .map(|n| json!({"name": n, "value": n}))
            .collect();
        self.call(
            Method::POST,
            &format!("/interactions/{interaction}/{token}/callback"),
            Some(json!({"type": 8, "data": {"choices": list}})),
        )
        .await
        .map(|_| ())
    }

    /// Acknowledges a button press without posting anything (the message is edited separately).
    pub async fn ack(&self, interaction: &str, token: &str) -> Result<()> {
        self.call(
            Method::POST,
            &format!("/interactions/{interaction}/{token}/callback"),
            Some(json!({"type": 6})),
        )
        .await
        .map(|_| ())
    }

    /// Replaces the bot's slash commands in one server.
    pub async fn register_commands(
        &self,
        app_id: &str,
        guild: &str,
        commands: Value,
    ) -> Result<()> {
        self.call(
            Method::PUT,
            &format!("/applications/{app_id}/guilds/{guild}/commands"),
            Some(commands),
        )
        .await
        .map(|_| ())
    }

    /// Up to 100 messages sent in a channel after message `after`, as Discord returns them (newest first, so callers sort).
    pub async fn messages_after(&self, channel: &str, after: &str) -> Result<Vec<Value>> {
        let v = self
            .call(
                Method::GET,
                &format!("/channels/{channel}/messages?after={after}&limit=100"),
                None,
            )
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// The newest messages of a channel (up to 100).
    pub async fn recent_messages(&self, channel: &str) -> Result<Vec<Value>> {
        let v = self
            .call(
                Method::GET,
                &format!("/channels/{channel}/messages?limit=100"),
                None,
            )
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// Deletes several messages at once (2 to 100, none older than two weeks: Discord's rule).
    pub async fn bulk_delete(&self, channel: &str, ids: &[String]) -> Result<()> {
        self.call(
            Method::POST,
            &format!("/channels/{channel}/messages/bulk-delete"),
            Some(json!({"messages": ids})),
        )
        .await
        .map(|_| ())
    }

    /// Deletes one message.
    pub async fn delete_message(&self, channel: &str, id: &str) -> Result<()> {
        self.call(
            Method::DELETE,
            &format!("/channels/{channel}/messages/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Downloads a file (an attachment's address).
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ApiError(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError(format!("download answered {}", resp.status())));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ApiError(e.to_string()))
    }
}

/// Whether text uses markdown (headings, lists, code blocks, bold, links), so that the attached file is named for it.
fn looks_like_markdown(t: &str) -> bool {
    t.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with('#')
            || l.starts_with("- ")
            || l.starts_with("* ")
            || l.starts_with("```")
            || l.starts_with("> ")
    }) || t.contains("**")
        || t.contains("](")
}

/// Discord allows 2000 characters in a message, and that is the only limit: text up to it is posted as it is. Only longer text is cut (with a note)
/// and returned to be attached as a file (`.md` when it uses markdown, else `.txt`). Returns (text to show, full text if it was cut).
pub fn split_for_discord(content: &str) -> (String, Option<String>) {
    const LIMIT: usize = 2000;
    if content.chars().count() <= LIMIT {
        return (content.to_string(), None);
    }
    let head: String = content.chars().take(1700).collect();
    (
        format!("{head}\n(too long for Discord: the full text is attached)"),
        Some(content.to_string()),
    )
}

/// What an agent wrote, made to read well in Discord: a shell leaves a literal `\n` where the agent meant a line break (when there is no real line
/// break at all, those are turned into real ones), and Discord does not draw markdown tables, so a table is put in a code block, where its columns line up.
pub fn tidy_for_discord(text: &str) -> String {
    let text = if !text.contains('\n') && text.contains("\\n") {
        text.replace("\\n", "\n")
    } else {
        text.to_string()
    };
    let is_row = |l: &str| {
        let t = l.trim();
        t.len() > 2 && t.starts_with('|') && t.ends_with('|')
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut i = 0;
    while i < lines.len() {
        if is_row(lines[i]) {
            let mut j = i;
            while j < lines.len() && is_row(lines[j]) {
                j += 1;
            }
            // Two or more rows in a row: a table.
            if j - i >= 2 {
                out.push("```".into());
                out.extend(lines[i..j].iter().map(|l| l.to_string()));
                out.push("```".into());
                i = j;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out.join("\n")
}
