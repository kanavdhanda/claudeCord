//! The tenant registry: one running hub per account, started the first time it is needed. Each tenant is the existing hub unchanged
//! (one pure core, one actor, one SQLite file in its own folder), so an account's data is separate from every other account's by
//! construction: there is no shared table to forget a `WHERE` on. The registry only decides which running hub a request belongs to,
//! and that decision comes from the control database (a machine's token or a dashboard session), never from anything the request says.
//!
//! It also runs each account's Discord bridges, one per saved bot. A bridge is given its bot's token opened from the sealed copy for
//! just that task, and the list of places the person chose on the dashboard.

use super::seal::{self, KeyProvider};
use super::{Account, Control};
use crate::discord::bridge::{self, BridgeConfig, Scope, Target, Targets};
use crate::hub::{Chat, HubCore};
use crate::server::{AppState, Config, Hub, spawn_core};
use crate::store::Store;
use crate::sync::Lock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// Whether text is a tenant id as `Control` makes them: a `t` and 16 hex digits. Checked before an id is used as a folder name, so a
/// crafted id can never point outside the data folder.
pub fn valid_tenant_id(id: &str) -> bool {
    id.len() == 17 && id.starts_with('t') && id[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where Discord is, so tests can point the bridges at a stand-in.
#[derive(Clone)]
pub struct DiscordSettings {
    pub api_base: String,
    pub gateway_url: Option<String>,
}

impl Default for DiscordSettings {
    fn default() -> Self {
        Self {
            api_base: "https://discord.com/api/v10".into(),
            gateway_url: None,
        }
    }
}

/// One account's running hub: what the front door serves it with, and the handle that stops it.
pub struct Tenant {
    pub(crate) state: AppState,
    id: String,
    account: Account,
    chat: broadcast::Sender<Chat>,
    hub: Mutex<Option<Hub>>,
    /// The running Discord bridges by bot id.
    bridges: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// Where an account's projects post, read from the control database each time it is asked, so a change on the dashboard takes effect
/// without restarting anything that has not been told to.
struct ControlTargets {
    control: Arc<Control>,
    tenant: String,
}

impl Targets for ControlTargets {
    fn target(&self, project: &str) -> Option<(String, Target)> {
        let p = self.control.target(&self.tenant, project).ok().flatten()?;
        Some((project.to_string(), as_target(p)))
    }

    fn mine(&self, bot: &str) -> Vec<(String, Target)> {
        self.control
            .targets(&self.tenant)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.bot == bot)
            .map(|p| (p.project.clone(), as_target(p)))
            .collect()
    }
}

fn as_target(p: super::Placement) -> Target {
    Target {
        bot: p.bot,
        guild: p.guild,
        channel: p.channel,
    }
}

/// The running hubs.
pub struct Registry {
    data: PathBuf,
    control: Arc<Control>,
    keys: Arc<dyn KeyProvider>,
    discord: DiscordSettings,
    /// Where old history and backups go, if the operator set up a bucket. Each account gets its own folder in it.
    bucket: Option<crate::store::bucket::Bucket>,
    cfg: Config,
    /// Discord people who own every account's projects here, besides each account's own person (for trying things out on one machine, where the
    /// made-up sign-in is not the person typing in Discord).
    extra_owners: Vec<String>,
    tenants: Mutex<HashMap<String, Arc<Tenant>>>,
}

impl Registry {
    /// A registry keeping each tenant's files under `data/tenants/<id>/`. `cfg` is the hub configuration every tenant starts with
    /// (its `bind` is never used: the gateway owns the port).
    pub fn new(
        data: PathBuf,
        control: Arc<Control>,
        cfg: Config,
        keys: Arc<dyn KeyProvider>,
        discord: DiscordSettings,
    ) -> Self {
        Self {
            data,
            control,
            keys,
            discord,
            bucket: None,
            cfg,
            extra_owners: Vec::new(),
            tenants: Mutex::new(HashMap::new()),
        }
    }

    /// Adds Discord people who own every account's projects (see the field).
    pub fn with_extra_owners(mut self, owners: Vec<String>) -> Self {
        self.extra_owners = owners;
        self
    }

    /// Uses a bucket for every account's old history and backups, each under `tenants/<id>/` so accounts never share a path.
    pub fn with_bucket(mut self, b: Option<crate::store::bucket::Bucket>) -> Self {
        self.bucket = b;
        self
    }

    /// The running hub of an account, started now if it is not running yet. Its owner is the account's Discord person.
    // ponytail: tenants are started on first use and never stopped until shutdown; add an idle sweep (stop hubs with no machine and
    // no dashboard use for a while) when the number of accounts is larger than memory allows.
    pub fn hub(&self, account: &Account) -> std::io::Result<Arc<Tenant>> {
        if !valid_tenant_id(&account.id) {
            return Err(std::io::Error::other("not a tenant id"));
        }
        let mut all = self.tenants.locked();
        if let Some(h) = all.get(&account.id) {
            return Ok(h.clone());
        }
        let dir = self.data.join("tenants").join(&account.id);
        std::fs::create_dir_all(&dir)?;
        let mut store = Store::open(&dir.join("hub.db"), Some(&dir.join("history")))
            .map_err(std::io::Error::other)?;
        store.set_bucket(self.bucket.clone().map(|mut b| {
            b.prefix = format!("{}tenants/{}/", b.prefix, account.id);
            b
        }));
        let mut core = HubCore::default();
        core.add_owner(&account.discord_id);
        for o in &self.extra_owners {
            core.add_owner(o);
        }
        let mut cfg = self.cfg.clone();
        cfg.log_path = None;
        let hub = spawn_core(cfg, core, store);
        let tenant = Arc::new(Tenant {
            state: hub.state.clone(),
            id: account.id.clone(),
            account: account.clone(),
            chat: hub.chat_sender(),
            hub: Mutex::new(Some(hub)),
            bridges: Mutex::new(HashMap::new()),
        });
        all.insert(account.id.clone(), tenant.clone());
        drop(all);
        crate::info!("hub", "started the hub of account {}", account.id);
        // Its saved bots come back up with it.
        for (bot, _, _) in self.control.bots(&account.id).unwrap_or_default() {
            self.start_bridge(&tenant, &bot);
        }
        Ok(tenant)
    }

    /// The running hub of the tenant with this id, if the account exists.
    pub fn hub_of(&self, tenant: &str) -> std::io::Result<Option<Arc<Tenant>>> {
        if !valid_tenant_id(tenant) {
            return Ok(None);
        }
        match self
            .control
            .account(tenant)
            .map_err(std::io::Error::other)?
        {
            Some(a) => self.hub(&a).map(Some),
            None => Ok(None),
        }
    }

    /// Starts (or restarts) the Discord bridge for one of an account's bots. The token is opened here, for this task only.
    // ponytail: placing a project restarts its bot's bridge so the new place is picked up (one gateway reconnect, and Discord allows
    // about 1000 per day per bot); tell the running bridge instead if people re-place projects often.
    pub fn start_bridge(&self, tenant: &Arc<Tenant>, bot: &str) {
        let Ok(Some(sealed)) = self.control.sealed_bot(&tenant.id, bot) else {
            return;
        };
        let app_id = self
            .control
            .bots(&tenant.id)
            .unwrap_or_default()
            .into_iter()
            .find(|(id, _, _)| id == bot)
            .map(|(_, app, _)| app);
        let Some(app_id) = app_id else { return };
        let Some(token) = seal::open(self.keys.as_ref(), &tenant.id, &app_id, &sealed) else {
            crate::error!(
                "discord",
                "cannot open the sealed token of bot {bot} of account {}: wrong or missing key",
                tenant.id
            );
            return;
        };
        let cfg = BridgeConfig {
            scope: Some(Scope {
                bot_id: bot.to_string(),
                targets: Arc::new(ControlTargets {
                    control: self.control.clone(),
                    tenant: tenant.id.clone(),
                }),
            }),
            api_base: self.discord.api_base.clone(),
            token,
            guild: String::new(),
            gateway_url: self.discord.gateway_url.clone(),
            db_path: self.data.join("tenants").join(&tenant.id).join("hub.db"),
            owners: std::iter::once(tenant.account.discord_id.clone())
                .chain(self.extra_owners.iter().cloned())
                .collect(),
            backoff_max: Duration::from_secs(60),
        };
        let handle = bridge::spawn(tenant.state.handle.clone(), tenant.chat.subscribe(), cfg);
        if let Some(old) = tenant.bridges.locked().insert(bot.to_string(), handle) {
            old.abort();
        }
    }

    /// Stops the Discord bridge of one bot.
    pub fn stop_bridge(&self, tenant: &Arc<Tenant>, bot: &str) {
        if let Some(h) = tenant.bridges.locked().remove(bot) {
            h.abort();
        }
    }

    /// Where Discord is, as this registry was told.
    pub fn discord(&self) -> &DiscordSettings {
        &self.discord
    }

    /// How many tenants are running now.
    pub fn running(&self) -> usize {
        self.tenants.locked().len()
    }

    /// Saves and stops every running hub, one after another.
    pub async fn shutdown(&self) {
        let all: Vec<Arc<Tenant>> = self.tenants.locked().drain().map(|(_, t)| t).collect();
        for t in all {
            for (_, h) in t.bridges.locked().drain() {
                h.abort();
            }
            let hub = t.hub.locked().take();
            if let Some(h) = hub {
                h.shutdown().await;
            }
        }
    }
}
