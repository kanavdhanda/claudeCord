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
    /// Allow listening on a public address without TLS. Tokens and messages would travel unencrypted.
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
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    // Everything the hub logs goes to the terminal and to `hub.log` in its data folder (kept to about 20 MB in two files).
    crate::log::init(Some(a.data.join("hub.log")));
    // Find the problems that can be found before serving anyone, and say them plainly, rather than failing in the middle of the night.
    preflight(&a.data)?;
    let store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
        .map_err(|e| e.to_string())?;
    let mut core = HubCore::default();
    for o in &a.owners {
        core.add_owner(o);
    }
    let oauth = super::discord::oauth_config(&a.data)?;
    let discord = super::discord::bridge_config(&a.data)?;
    let hub = server::start(
        Config {
            bind,
            oauth,
            discord_expected: discord.is_some(),
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
    // Discord is part of the hub: once `claudecord discord set` has been run, starting the hub starts the bridge too.
    match discord {
        Some(mut cfg) => {
            cfg.owners = a.owners.clone();
            crate::discord::bridge::spawn(hub.handle(), hub.chat(), cfg);
            crate::info!("hub", "the Discord bridge is starting");
        }
        None => crate::warn!(
            "hub",
            "Discord is not set up (claudecord discord set), so only the dashboard and machines are served"
        ),
    }
    crate::info!(
        "hub",
        "listening on {}  (data in {}, log in hub.log)",
        hub.addr,
        a.data.display()
    );
    // Ctrl-C, and on Unix SIGTERM (what `systemctl stop` and Docker send): either way the hub saves everything and records a clean stop.
    crate::task::shutdown_signal().await;
    crate::info!("hub", "told to stop; saving and closing connections");
    hub.shutdown().await;
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
