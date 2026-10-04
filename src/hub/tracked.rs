//! Collections that remember what was changed in them, so saving state costs what changed and not how much state there is.
//!
//! The hub's durable state is a set of maps keyed by project or agent. Saving all of it after every batch would cost more the more
//! projects there are, so each map here records which of its keys were touched, and a save writes exactly those entries (a few hundred
//! bytes each) in the same transaction as the batch's history. Because the tracking is part of the types, no code that changes state can
//! forget to say so: a mutating method either marks the key it touches or marks the whole collection, and reading never marks anything.
//!
//! Each entry becomes one database row named `collection:key` (a whole-value cell is one row named for itself).

use serde::Serialize;
use std::collections::{HashMap, HashSet, hash_map};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

/// What has been touched since the last save.
#[derive(Default)]
pub(super) struct DirtyLog {
    /// Entries touched, as (collection, key).
    pub keys: HashSet<(&'static str, String)>,
    /// Collections touched as a whole (cleared, filtered, or changed through a general borrow): all of their rows are rewritten.
    pub whole: HashSet<&'static str>,
}

/// The log shared by every tracked collection of one core.
pub(super) type Log = Arc<Mutex<DirtyLog>>;

fn mark_key(log: &Log, name: &'static str, key: &str) {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys
        .insert((name, key.to_string()));
}

fn mark_whole(log: &Log, name: &'static str) {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .whole
        .insert(name);
}

/// A map from a name to a value that notes which names were changed.
pub(super) struct TrackedMap<V> {
    name: &'static str,
    log: Log,
    map: HashMap<String, V>,
}

impl<V> TrackedMap<V> {
    pub(super) fn new(name: &'static str, log: &Log) -> Self {
        Self {
            name,
            log: log.clone(),
            map: HashMap::new(),
        }
    }

    /// Swaps in a whole new map without counting it as a change (used when loading saved state).
    pub(super) fn load(&mut self, map: HashMap<String, V>) {
        self.map = map;
    }

    pub(super) fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        mark_key(&self.log, self.name, key);
        self.map.get_mut(key)
    }

    pub(super) fn insert(&mut self, key: String, value: V) -> Option<V> {
        mark_key(&self.log, self.name, &key);
        self.map.insert(key, value)
    }

    pub(super) fn remove(&mut self, key: &str) -> Option<V> {
        mark_key(&self.log, self.name, key);
        self.map.remove(key)
    }

    pub(super) fn entry(&mut self, key: String) -> hash_map::Entry<'_, String, V> {
        mark_key(&self.log, self.name, &key);
        self.map.entry(key)
    }

    pub(super) fn retain(&mut self, f: impl FnMut(&String, &mut V) -> bool) {
        mark_whole(&self.log, self.name);
        self.map.retain(f);
    }

    pub(super) fn values_mut(&mut self) -> hash_map::ValuesMut<'_, String, V> {
        mark_whole(&self.log, self.name);
        self.map.values_mut()
    }

    /// The rows for the touched entries: (key, JSON) for those that exist, and the keys of those that no longer do.
    pub(super) fn rows(&self, touched: &[String]) -> (Vec<(String, String)>, Vec<String>)
    where
        V: Serialize,
    {
        let (mut up, mut del) = (Vec::new(), Vec::new());
        for k in touched {
            match self.map.get(k) {
                Some(v) => up.push((
                    format!("{}:{k}", self.name),
                    serde_json::to_string(v).expect("state is plain data"),
                )),
                None => del.push(format!("{}:{k}", self.name)),
            }
        }
        (up, del)
    }

    /// Every row, for rewriting the whole collection.
    pub(super) fn all_rows(&self) -> Vec<(String, String)>
    where
        V: Serialize,
    {
        self.map
            .iter()
            .map(|(k, v)| {
                (
                    format!("{}:{k}", self.name),
                    serde_json::to_string(v).expect("state is plain data"),
                )
            })
            .collect()
    }
}

impl<V> Deref for TrackedMap<V> {
    type Target = HashMap<String, V>;
    fn deref(&self) -> &HashMap<String, V> {
        &self.map
    }
}

/// A set of names that notes which were added or removed.
pub(super) struct TrackedSet {
    name: &'static str,
    log: Log,
    set: HashSet<String>,
}

impl TrackedSet {
    pub(super) fn new(name: &'static str, log: &Log) -> Self {
        Self {
            name,
            log: log.clone(),
            set: HashSet::new(),
        }
    }

    pub(super) fn load(&mut self, set: HashSet<String>) {
        self.set = set;
    }

    pub(super) fn insert(&mut self, key: String) -> bool {
        mark_key(&self.log, self.name, &key);
        self.set.insert(key)
    }

    pub(super) fn remove(&mut self, key: &str) -> bool {
        mark_key(&self.log, self.name, key);
        self.set.remove(key)
    }

    pub(super) fn rows(&self, touched: &[String]) -> (Vec<(String, String)>, Vec<String>) {
        let (mut up, mut del) = (Vec::new(), Vec::new());
        for k in touched {
            if self.set.contains(k) {
                up.push((format!("{}:{k}", self.name), "1".to_string()));
            } else {
                del.push(format!("{}:{k}", self.name));
            }
        }
        (up, del)
    }

    pub(super) fn all_rows(&self) -> Vec<(String, String)> {
        self.set
            .iter()
            .map(|k| (format!("{}:{k}", self.name), "1".to_string()))
            .collect()
    }
}

impl Deref for TrackedSet {
    type Target = HashSet<String>;
    fn deref(&self) -> &HashSet<String> {
        &self.set
    }
}

/// A value saved as one row, which counts as changed whenever it is borrowed to be changed.
pub(super) struct Cell<T> {
    name: &'static str,
    log: Log,
    value: T,
}

impl<T> Cell<T> {
    pub(super) fn new(name: &'static str, log: &Log, value: T) -> Self {
        Self {
            name,
            log: log.clone(),
            value,
        }
    }

    pub(super) fn row(&self) -> (String, String)
    where
        T: Serialize,
    {
        (
            self.name.to_string(),
            serde_json::to_string(&self.value).expect("state is plain data"),
        )
    }
}

impl<T> Deref for Cell<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Cell<T> {
    fn deref_mut(&mut self) -> &mut T {
        mark_whole(&self.log, self.name);
        &mut self.value
    }
}
