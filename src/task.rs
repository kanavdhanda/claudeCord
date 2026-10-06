//! Keeping long-running work alive: a supervisor that restarts a task if it panics, and the signal that means "stop now".
//!
//! A panic in a spawned task ends only that task, silently. For work the whole program depends on (the Discord bridge, for example)
//! that means a part of the system quietly stops while everything else looks fine. `supervised` logs the panic and starts the task
//! again after a pause that grows from one second to thirty, and gives up only when the task ends on its own.

use std::future::Future;
use std::time::Duration;

/// Aborts the task it was made for when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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
            // Aborting this supervisor must end the task it runs too: a dropped JoinHandle only lets go of it, and the old task would keep
            // running next to its replacement (two Discord bridges for one bot post every message twice).
            let inner = tokio::spawn(make());
            let _ends_with_us = AbortOnDrop(inner.abort_handle());
            match inner.await {
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

/// Listens for the request to stop: Ctrl-C everywhere, and on Unix also SIGTERM, which is what `systemctl stop`, Docker and most process
/// managers send. Making it registers the handlers at once, so a stop asked for while the program is still starting is not lost to the
/// default action (which would kill it without saving anything): it waits to be noticed. Handling it means a normal stop saves everything
/// and is recorded as a stop, not a crash.
pub struct StopListener {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
}

/// Starts listening now. Call it first thing, inside the runtime.
pub fn stop_listener() -> StopListener {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        StopListener {
            term: signal(SignalKind::terminate()).ok(),
            int: signal(SignalKind::interrupt()).ok(),
        }
    }
    #[cfg(not(unix))]
    {
        StopListener {}
    }
}

impl StopListener {
    /// Completes when the program has been asked to stop (possibly already, before this was called).
    pub async fn wait(self) {
        #[cfg(unix)]
        {
            let (mut term, mut int) = (self.term, self.int);
            async fn on(s: &mut Option<tokio::signal::unix::Signal>) {
                match s {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending().await,
                }
            }
            tokio::select! {
                _ = on(&mut term) => {}
                _ = on(&mut int) => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Completes when the program is asked to stop (see `StopListener`).
pub async fn shutdown_signal() {
    stop_listener().wait().await;
}

/// Runs one step and survives a panic in it: the panic is logged (with `what` was being done) and the result is None, so the caller
/// can report it and carry on with the next thing instead of the whole program going down with one bad step.
pub async fn guarded<T>(what: &str, step: impl Future<Output = T>) -> Option<T> {
    use futures_util::FutureExt;
    match std::panic::AssertUnwindSafe(step).catch_unwind().await {
        Ok(v) => Some(v),
        Err(_) => {
            crate::error!(
                "task",
                "a panic while {what}; that step was dropped and the program carries on"
            );
            None
        }
    }
}
