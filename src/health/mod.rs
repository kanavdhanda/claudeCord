//! The health check: boots the real parts of the system in this process and exercises every feature, to prove each one
//! is not just present but wired up and working. It is what `claudecord selftest` runs, what CI runs after the unit
//! tests, and what you can run on a new machine to see that everything is alive.
//!
//! Each feature has a probe that does something a user would depend on and reports the evidence it saw. A feature that
//! compiles but is not connected to anything fails its probe. A test (tests/health.rs) also fails if a source file is
//! not claimed by any probe, so a new module cannot be added without a check that it is alive.
//!
//! Files: this one holds the registry and the report; `probes_core` checks the hub's rules and storage; `probes_live`
//! checks the running pieces (server, link, terminals, daemon).

mod probes_core;
mod probes_live;

use futures_util::FutureExt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

/// What a probe returns: what it saw (on success) or what went wrong.
pub type Probe = Pin<Box<dyn Future<Output = Result<String, String>>>>;

/// One feature to check.
pub struct Feature {
    /// What a person would call it.
    pub name: &'static str,
    /// The source files (as paths under `src/`, without `.rs`) this feature's probe exercises. A path stands for every
    /// file below it too.
    pub covers: &'static [&'static str],
    pub probe: fn() -> Probe,
}

/// The result of checking one feature.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Outcome {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub millis: u128,
}

/// Every feature, in the order they are checked.
pub fn features() -> Vec<Feature> {
    let mut all = probes_core::features();
    all.extend(probes_live::features());
    all
}

/// Runs every probe and collects the outcomes. A probe that panics counts as failed.
pub async fn run_all() -> Vec<Outcome> {
    let mut out = Vec::new();
    for f in features() {
        let started = Instant::now();
        let result = std::panic::AssertUnwindSafe((f.probe)())
            .catch_unwind()
            .await;
        let (ok, detail) = match result {
            Ok(Ok(evidence)) => (true, evidence),
            Ok(Err(why)) => (false, why),
            Err(_) => (false, "the check crashed".to_string()),
        };
        out.push(Outcome {
            name: f.name,
            ok,
            detail,
            millis: started.elapsed().as_millis(),
        });
    }
    out
}

/// A scratch folder for a probe, emptied first.
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cc-health-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp folder");
    d
}

/// Waits up to a few seconds for something to become true.
pub(crate) async fn eventually(mut f: impl AsyncFnMut() -> bool) -> bool {
    // Generous: a shared CI machine can be slow, and a wait only lasts as long as the thing it waits for.
    for _ in 0..800 {
        if f().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// Boxes an async block as a probe.
pub(crate) fn boxed(f: impl Future<Output = Result<String, String>> + 'static) -> Probe {
    Box::pin(f)
}
