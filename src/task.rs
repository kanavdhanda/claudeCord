//! Keeping long-running work alive: a supervisor that restarts a task if it panics, and the signal that means "stop now".
//!
//! A panic in a spawned task ends only that task, silently. For work the whole program depends on (the Discord bridge, for example)
//! that means a part of the system quietly stops while everything else looks fine. `supervised` logs the panic and starts the task
//! again after a pause that grows from one second to thirty, and gives up only when the task ends on its own.

use std::future::Future;
use std::time::Duration;

/// Runs `make()` as a task and runs it again, after a growing pause, each time it panics. Returns when the task returns normally.
pub fn supervised<F, Fut>(name: &'static str, mut make: F) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut pause = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            match tokio::spawn(make()).await {
                Ok(()) => return,
                Err(e) if e.is_panic() => {
                    crate::error!(name, "stopped by a panic; starting it again in {pause:?}");
                }
                Err(_) => return, // cancelled: the runtime is shutting down
            }
            tokio::time::sleep(pause).await;
            // A task that ran for a good while before failing starts over with the short pause.
            pause = if started.elapsed() > Duration::from_secs(60) {
                Duration::from_secs(1)
            } else {
                (pause * 2).min(Duration::from_secs(30))
            };
        }
    })
}

/// Completes when the program is asked to stop: Ctrl-C everywhere, and on Unix also SIGTERM, which is what `systemctl stop`, Docker
/// and most process managers send. Handling it means a normal stop saves everything and is recorded as a stop, not a crash.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
