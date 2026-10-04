//! A lock that survives a panic elsewhere. A standard mutex becomes "poisoned" if a thread panics while holding it, and every
//! later `lock().expect(..)` then panics too, so one bug in one place would take down everything that shares the lock. What the
//! lock protects here is plain state that stays usable after a panic (the hub's core rebuilds itself from its last save anyway), so
//! `locked` takes the guard back and carries on.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `mutex.locked()` instead of `mutex.lock().expect(..)`.
pub trait Lock<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Lock<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
