//! `claudecord-hub uptime` shows how much of the time each part was working; `claudecord-hub probe` checks a hub from the outside
//! (run it on a different machine) and keeps its own record of what it saw. See `crate::uptime` for how it is worked out.

use crate::store::Store;
use crate::uptime::{self, Debounce};
use clap::Args;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct UptimeArgs {
    /// Where the hub (or the prober) keeps its files.
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
    /// The availability target, in percent, that the error budget is measured against.
    #[arg(long, default_value_t = 99.9)]
    pub target: f64,
}

#[derive(Args)]
pub struct ProbeArgs {
    /// The hub's public address, such as https://hub.example.com
    pub url: String,
    /// Where the prober keeps its record (it never needs the hub's files).
    #[arg(long, default_value = "claudecord-probe")]
    pub data: PathBuf,
    /// A name for this prober, such as its region. It is recorded as `external:NAME`.
    #[arg(long, default_value = "probe")]
    pub name: String,
    /// Seconds between checks.
    #[arg(long, default_value_t = 30)]
    pub every: u64,
    /// Failures in a row before it counts as an outage.
    #[arg(long, default_value_t = 3)]
    pub failures: u32,
    /// Check once, print the answer and exit 0 (up) or 1 (down). For cron and CI schedules.
    #[arg(long)]
    pub once: bool,
}

/// "99.95%" or "no data".
pub fn percent(a: Option<f64>) -> String {
    a.map_or("no data".into(), |a| format!("{:.3}%", a * 100.0))
}

/// "1h 05m", "42s".
pub fn span(ms: i64) -> String {
    let s = ms.abs() / 1000;
    let t = if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    };
    if ms < 0 { format!("-{t}") } else { t }
}

/// Prints availability, error budget and the latest outages for every component.
pub fn show(a: UptimeArgs) -> Result<(), String> {
    let db = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let now = crate::now_ms();
    let target = a.target / 100.0;
    let components = db.uptime_components().map_err(|e| e.to_string())?;
    if components.is_empty() {
        println!("nothing recorded yet: the hub records itself while it runs");
    }
    for c in components {
        let state = db
            .uptime_last(&c)
            .ok()
            .flatten()
            .map_or("unknown", uptime::State::as_str);
        println!("{c}  (now {state})");
        for (name, len) in crate::server::health::WINDOWS {
            let changes = db
                .uptime_changes(&c, now - len)
                .map_err(|e| e.to_string())?;
            let r = uptime::report(&changes, now - len, now);
            println!(
                "  {name:>3}  {:>9}   down {:>8}   budget left {:>9} (target {}%)",
                percent(r.availability()),
                span(r.down_ms),
                span(uptime::budget_left(&r, target)),
                a.target
            );
        }
        let week = 7 * 24 * 3_600_000;
        let changes = db
            .uptime_changes(&c, now - week)
            .map_err(|e| e.to_string())?;
        for (from, to) in uptime::outages(&changes, now - week, now)
            .iter()
            .rev()
            .take(5)
        {
            println!(
                "  outage  {} to {}  ({})",
                uptime::iso(*from),
                uptime::iso(*to),
                span(to - from)
            );
        }
    }
    Ok(())
}

/// Checks the hub over and over, or once.
pub async fn probe(a: ProbeArgs) -> Result<(), String> {
    if a.once {
        let up = uptime::probe_once(&a.url, Duration::from_secs(10)).await;
        println!("{} {}", a.url, if up { "up" } else { "down" });
        return if up {
            Ok(())
        } else {
            Err("the hub did not answer /readyz with 200".into())
        };
    }
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let db = Store::open(&a.data.join("hub.db"), None).map_err(|e| e.to_string())?;
    let component = format!("external:{}", a.name);
    let mut debounce = Debounce::new(a.failures);
    println!(
        "checking {} every {}s as {component}; Ctrl-C stops",
        a.url, a.every
    );
    loop {
        let ok = uptime::probe_once(&a.url, Duration::from_secs(10)).await;
        if let Some(c) = debounce.check(ok, crate::now_ms()) {
            let _ = db.uptime_set(&component, c.state, c.at);
            println!("{}  {}", uptime::iso(c.at), c.state.as_str());
        }
        tokio::time::sleep(Duration::from_secs(a.every)).await;
    }
}
