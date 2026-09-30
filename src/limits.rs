//! Small rate limiters. Both take the time as an argument, so they are deterministic to test.

use std::collections::HashMap;

/// Token bucket. Allows a short burst and a sustained rate.
pub struct Bucket {
    capacity: f64,
    per_second: f64,
    tokens: f64,
    last: f64,
}

impl Bucket {
    pub fn new(capacity: f64, per_second: f64, now: f64) -> Self {
        Self {
            capacity,
            per_second,
            tokens: capacity,
            last: now,
        }
    }

    pub fn take(&mut self, n: f64, now: f64) -> bool {
        // Never let a clock that steps backwards drain the bucket.
        let elapsed = (now - self.last).max(0.0);
        self.tokens = self
            .capacity
            .min(self.tokens + elapsed / 1000.0 * self.per_second);
        self.last = self.last.max(now);
        if self.tokens < n {
            return false;
        }
        self.tokens -= n;
        true
    }
}

/// Counts failures per key inside a window, so repeated bad tokens are refused without a database lookup.
pub struct FailureLimiter {
    max: u32,
    window_ms: f64,
    hits: HashMap<String, (u32, f64)>,
}

impl FailureLimiter {
    pub fn new(max: u32, window_ms: f64) -> Self {
        Self {
            max,
            window_ms,
            hits: HashMap::new(),
        }
    }

    pub fn blocked(&mut self, key: &str, now: f64) -> bool {
        match self.hits.get(key) {
            None => false,
            Some(&(_, reset)) if now > reset => {
                self.hits.remove(key);
                false
            }
            Some(&(n, _)) => n >= self.max,
        }
    }

    pub fn fail(&mut self, key: &str, now: f64) {
        match self.hits.get_mut(key) {
            Some((n, reset)) if now <= *reset => *n += 1,
            _ => {
                self.hits.insert(key.to_string(), (1, now + self.window_ms));
            }
        }
        // Keep the map from growing without bound under a spray of addresses.
        if self.hits.len() > 10_000 {
            self.hits.retain(|_, (_, reset)| now <= *reset);
        }
    }
}
