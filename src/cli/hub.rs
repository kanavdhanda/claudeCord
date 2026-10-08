//! The server commands: `claudecord hub` runs the central hub, `claudecord token` makes a token for a machine, and
//! `claudecord load-tokens` makes many at once into a private file for the k6 load test.

use crate::hub::HubCore;
use crate::server::{self, Config};
use crate::store::Store;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct HubArgs {
    /// Where the hub keeps its files.
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
    /// Address to listen on. Put a TLS proxy (Caddy, nginx, Cloudflare) in front for anything but this machine.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub bind: String,
    /// Chat account ids of the owners (repeat for several).
    #[arg(long = "owner")]
    pub owners: Vec<String>,
    /// Keep an Obsidian vault of the conversation up to date in this folder while the hub runs (open it in Obsidian), and once more when
    /// the hub stops. The same thing `claudecord export` writes.
    #[arg(long)]
    pub vault: Option<PathBuf>,
    /// How often the vault is refreshed, in seconds.
    #[arg(long, default_value_t = 30)]
    pub vault_every: u64,
    /// Allow listening on a public address without TLS. Tokens and messages would travel unencrypted.
    #[arg(long)]
    pub allow_plain: bool,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Where the service keeps its files (the control database, the key file, and a folder per account).
    #[arg(long, default_value = "claudecord-data")]
    pub data: PathBuf,
    /// Address to listen on. Put a TLS proxy (Caddy) in front and keep this on 127.0.0.1.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub bind: String,
    /// The address people open in a browser, such as https://claudecord.example.com. Sign-in comes back to `<this>/auth/callback`.
    #[arg(long)]
    pub public_url: String,
    /// The Discord application used to sign people in (not their bots). Create it in the Discord developer portal.
    #[arg(long, required_unless_present_any = ["dev", "demo"])]
    pub client_id: Option<String>,
    /// File holding that application's client secret.
    #[arg(long, required_unless_present_any = ["dev", "demo"])]
    pub secret_file: Option<PathBuf>,
    /// For working on this program on your own machine: sign-in lets you in as a made-up person (there is no Discord login app to set up locally).
    /// Everything else is real: bots must be real Discord bot tokens, checked with Discord, and there is no pre-filled data. Only runs on 127.0.0.1.
    #[arg(long, hide = true)]
    pub dev: bool,
    /// A Discord user id that owns every account's projects here (repeat for several), besides the account's own person. For `--dev`, where the
    /// made-up sign-in is not the Discord person typing in the channel: put YOUR Discord user id here (Discord: Settings, Advanced, Developer Mode,
    /// then right-click your name and Copy User ID). The hub's log says it when it ignores someone, with their id.
    #[arg(long = "owner-id", hide = true)]
    pub owner_ids: Vec<String>,
    /// A look around with no Discord at all (implies --dev): a stand-in Discord keeps every page working with no account and no bot, and the
    /// dashboard is pre-filled with a made-up project, machine and agents. Only runs on 127.0.0.1.
    #[arg(long, hide = true)]
    pub demo: bool,
    /// Allow listening on a public address without TLS.
    #[arg(long)]
    pub allow_plain: bool,
}

#[derive(Args)]
pub struct TokenArgs {
    /// The machine's name.
    pub node: String,
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
}

/// Makes a token for the dashboard and prints it once. Open the hub's address in a browser and paste it (or add
/// `#token=...` to the address). Only its hash is kept, and it opens the dashboard and nothing else.
pub fn make_web_token(a: TokenArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let mut store = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let token = store
        .create_token(
            &format!("{}{}", crate::server::web::WEB_PREFIX, a.node),
            crate::now_ms(),
        )
        .map_err(|e| e.to_string())?;
    println!("{token}");
    eprintln!(
        "Save this now. It cannot be shown again. Open the hub in a browser and paste it, or add #token=<it> to the address."
    );
    Ok(())
}

/// Runs the hub until interrupted, then saves and tells every machine it is going away.
pub async fn run_hub(a: HubArgs) -> Result<(), String> {
    let bind: std::net::SocketAddr = a
        .bind
        .parse()
        .map_err(|_| format!("{} is not an address like 127.0.0.1:8787", a.bind))?;
    if !bind.ip().is_loopback() && !a.allow_plain {
        return Err("refusing to listen on a public address without TLS: put a TLS proxy in front and bind to 127.0.0.1, or pass --allow-plain if you really mean it".into());
    }
    // First of all, so a stop asked for while the hub is still starting waits to be handled instead of killing it unsaved.
    let stop = crate::task::stop_listener();
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    // Everything the hub logs goes to the terminal and to `hub.log` in its data folder (kept to about 20 MB in two files).
    crate::log::init(Some(a.data.join("hub.log")));
    // Find the problems that can be found before serving anyone, and say them plainly, rather than failing in the middle of the night.
    preflight(&a.data)?;
    let mut store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
        .map_err(|e| e.to_string())?;
    // Old history and backups go to the bucket chosen with `claudecord storage`, if there is one.
    if a.data.join("storage.json").exists() {
        store.set_bucket(Some(super::storage::load(&a.data.join("storage.json"))?));
    }
    let mut core = HubCore::default();
    for o in &a.owners {
        core.add_owner(o);
    }
    // No Discord here: bot tokens are only ever saved through the hosted service, sealed (`claudecord serve`). This hub is for tests and development.
    let hub = server::start(
        Config {
            bind,
            log_path: Some(a.data.join("hub.log")),
            ..Config::default()
        },
        core,
        store,
    )
    .await
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => format!("{bind} is already in use: is another hub running here? Stop it, or choose another --bind"),
        std::io::ErrorKind::PermissionDenied => format!("not allowed to listen on {bind} (ports below 1024 need extra permission; use a higher port behind a TLS proxy)"),
        _ => e.to_string(),
    })?;
    crate::info!(
        "hub",
        "listening on {}  (data in {}, log in hub.log)",
        hub.addr,
        a.data.display()
    );
    // Now systemd (if it started the hub with Type=notify) is told the hub is serving, and the vault, if asked for, starts being kept.
    crate::notify::ready();
    let vault = a
        .vault
        .clone()
        .map(|out| VaultKeeper::start(a.data.clone(), out, a.vault_every));
    // Ctrl-C, and on Unix SIGTERM (what `systemctl stop` and Docker send): either way the hub saves everything and records a clean stop.
    stop.wait().await;
    crate::notify::stopping();
    crate::info!("hub", "told to stop; saving and closing connections");
    hub.shutdown().await;
    // The vault gets one last refresh, now that everything is saved.
    if let Some(v) = vault {
        v.finish();
    }
    crate::info!("hub", "stopped cleanly");
    Ok(())
}

/// Checks what can be checked before the hub starts serving: the data folder can be written, the database is not damaged, and the
/// process may open as many connections as devices will need. A problem that stops the hub is an error with the fix in it; one
/// that only limits it is a warning in the log.
pub fn preflight(data: &std::path::Path) -> Result<(), String> {
    let probe = data.join(".write-test");
    std::fs::write(&probe, b"ok").map_err(|e| {
        format!(
            "the data folder {} cannot be written to: {e}",
            data.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    let db = data.join("hub.db");
    if db.exists() {
        let store = Store::open(&db, None)
            .map_err(|e| format!("the database {} cannot be opened: {e}", db.display()))?;
        store.integrity().map_err(|found| {
            format!(
                "the database {} is damaged ({found}). Bring back the last copy with `claudecord storage restore`, or move the file aside to start fresh",
                db.display()
            )
        })?;
    }
    // Every device is one open file for the hub. Linux says what the limit is; elsewhere there is nothing cheap to ask.
    if let Some(limit) = std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("Max open files"))?
                .split_whitespace()
                .nth(3)?
                .parse::<u64>()
                .ok()
        })
        && limit < 4096
    {
        crate::warn!(
            "hub",
            "this process may open only {limit} files, which limits how many devices can connect; raise it (LimitNOFILE in the systemd unit, or ulimit -n)"
        );
    }
    Ok(())
}

/// Makes a token for a machine and prints it once. Only its hash is kept.
pub fn make_token(a: TokenArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let mut store = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let token = store
        .create_token(&a.node, crate::now_ms())
        .map_err(|e| e.to_string())?;
    println!("{token}");
    eprintln!(
        "Save this now. It cannot be shown again. On the machine: claudecord login --hub wss://YOUR-HUB --token <it> --name {}",
        a.node
    );
    Ok(())
}

#[derive(Args)]
pub struct LoadTokensArgs {
    /// How many machines (one per simulated user).
    #[arg(long)]
    pub count: usize,
    /// Machine names are PREFIX-1, PREFIX-2, ...
    #[arg(long, default_value = "load")]
    pub prefix: String,
    /// The private JSON file the tokens are written to: [{"node": "load-1", "token": "..."}].
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
}

/// Makes `count` machine tokens at once for load testing. They are written to a file only the owner can read, never printed.
pub fn make_load_tokens(a: LoadTokensArgs) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let mut store = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let now = crate::now_ms();
    let mut rows = Vec::with_capacity(a.count);
    for i in 1..=a.count {
        let node = format!("{}-{i}", a.prefix);
        let token = store.create_token(&node, now).map_err(|e| e.to_string())?;
        rows.push(serde_json::json!({"node": node, "token": token}));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&a.out).map_err(|e| e.to_string())?;
    f.write_all(
        serde_json::to_string(&rows)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    eprintln!("wrote {} tokens to {}", a.count, a.out.display());
    Ok(())
}

/// Keeps an Obsidian vault up to date from a thread of its own (it reads the hub's files and never changes them), and once more at the end.
struct VaultKeeper {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    args: super::export::ExportArgs,
}

impl VaultKeeper {
    fn start(data: PathBuf, out: PathBuf, every: u64) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let args = || super::export::ExportArgs {
            data: data.clone(),
            out: out.clone(),
            project: None,
            watch: None,
        };
        let (flag, a) = (stop.clone(), args());
        let thread = std::thread::spawn(move || {
            crate::info!(
                "hub",
                "keeping an Obsidian vault up to date in {} (every {every} s)",
                a.out.display()
            );
            let mut waited = 0u64;
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                if waited == 0
                    && let Err(e) = super::export::export_once(&a)
                {
                    crate::warn!("hub", "could not update the Obsidian vault: {e}");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                waited = (waited + 1) % every.max(1);
            }
        });
        Self {
            stop,
            thread,
            args: args(),
        }
    }

    fn finish(self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.thread.join();
        if let Err(e) = super::export::export_once(&self.args) {
            crate::warn!("hub", "could not update the Obsidian vault at the end: {e}");
        }
    }
}

/// Runs the hosted service: one front door for many accounts. People sign in with Discord, machines join by a code, and each account's
/// data lives in its own folder under `--data`.
pub async fn run_serve(a: ServeArgs) -> Result<(), String> {
    use crate::control::{Control, seal::LocalKeys};
    let bind: std::net::SocketAddr = a
        .bind
        .parse()
        .map_err(|_| format!("{} is not an address like 127.0.0.1:8787", a.bind))?;
    if !bind.ip().is_loopback() && !a.allow_plain {
        return Err("refusing to listen on a public address without TLS: put Caddy in front and bind to 127.0.0.1, or pass --allow-plain".into());
    }
    let url = a.public_url.trim_end_matches('/').to_string();
    if !url.starts_with("https://")
        && !url.starts_with("http://localhost")
        && !url.starts_with("http://127.0.0.1")
    {
        return Err("--public-url must be https:// (plain http only for localhost)".into());
    }
    let oauth = match (&a.client_id, &a.secret_file) {
        (Some(id), Some(f)) => {
            let secret = std::fs::read_to_string(f)
                .map_err(|e| format!("cannot read {}: {e}", f.display()))?
                .trim()
                .to_string();
            if secret.is_empty() || id.trim().is_empty() {
                return Err("the client id and secret must not be empty".into());
            }
            Some(server::Oauth {
                client_id: id.clone(),
                client_secret: secret,
                redirect_uri: format!("{url}/auth/callback"),
                authorize_url: "https://discord.com/oauth2/authorize".into(),
                api_base: "https://discord.com/api/v10".into(),
            })
        }
        _ => None,
    };
    // Local preview: a stand-in Discord keeps every page of the wizard working with no account and no bot.
    let discord = if a.demo {
        let (fake, addr) = crate::discord::fake::start_fake().await;
        std::mem::forget(fake);
        crate::info!(
            "hub",
            "demo mode: a stand-in Discord is running, nothing here reaches the real one"
        );
        crate::control::registry::DiscordSettings {
            api_base: format!("http://{addr}/api"),
            gateway_url: None,
        }
    } else {
        Default::default()
    };
    if !a.owner_ids.is_empty() && !(a.dev || a.demo) {
        return Err("--owner-id is only for --dev or --demo (it is for working on claudeCord on one machine)".into());
    }
    if let Some(bad) = a
        .owner_ids
        .iter()
        .find(|o| o.is_empty() || !o.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "--owner-id is a Discord user id, digits only (got {bad:?})"
        ));
    }
    let stop = crate::task::stop_listener();
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    crate::log::init(Some(a.data.join("hub.log")));
    // The operator's bucket (`claudecord storage oracle ...` with this --data) holds old history and backups for every account.
    let bucket = if a.data.join("storage.json").exists() {
        Some(super::storage::load(&a.data.join("storage.json"))?)
    } else {
        None
    };
    let control = std::sync::Arc::new(
        Control::open(&a.data.join("control.db")).map_err(|e| format!("control database: {e}"))?,
    );
    // The key that seals bot tokens. Made on first start; back this file up, because without it every saved token is lost.
    let keys: std::sync::Arc<dyn crate::control::seal::KeyProvider> =
        std::sync::Arc::new(LocalKeys::load_or_create(&a.data.join("kek"))?);
    let g = server::gateway::start_gateway(
        server::gateway::GatewayConfig {
            bind,
            public_url: url.clone(),
            oauth,
            hub: Config::default(),
            discord,
            dev: a.dev || a.demo,
            bucket,
            extra_owners: a.owner_ids.clone(),
        },
        a.data.clone(),
        control,
        keys.clone(),
    )
    .await
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            format!("{bind} is already in use: is another copy running?")
        }
        _ => e.to_string(),
    })?;
    if a.demo {
        // So the dashboard has something to show: a made-up account with a placed project, a machine, agents and a few hours of work.
        server::demo::seed(&g.control, &g.registry, keys.as_ref())
            .await
            .map_err(|e| format!("could not make the demo data: {e}"))?;
        crate::info!("hub", "demo mode: sign in to see a demo account");
    }
    crate::info!(
        "hub",
        "serving accounts on {} (public address {url})",
        g.addr
    );
    crate::notify::ready();
    stop.wait().await;
    crate::notify::stopping();
    crate::info!("hub", "told to stop; saving every account's hub");
    g.shutdown().await;
    crate::info!("hub", "stopped cleanly");
    Ok(())
}
