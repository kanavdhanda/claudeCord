//! What a machine remembers between runs: the hub address, its token, and its name. Stored as a small JSON file in the
//! user's own folder, readable by that user only, because the token lets anyone who has it act as this machine.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Where the hub listens, for example `wss://hub.example.com`.
    pub hub_url: String,
    pub token: String,
    /// This machine's name, shown to people and used to tell machines apart.
    pub node_name: String,
}

/// The folder claudeCord keeps its files in: `$CLAUDECORD_HOME` if set, otherwise `~/.claudecord`.
pub fn home_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDECORD_HOME") {
        return PathBuf::from(p);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".claudecord")
}

impl Config {
    /// Reads the saved config from `dir`, or None if there is none or it is unreadable.
    pub fn load(dir: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).ok()?).ok()
    }

    /// Saves the config into `dir`, creating the folder if needed, readable by the owner only.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(self).expect("plain data"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::rename(&tmp, &path)
    }

    /// The WebSocket address of the hub's device door.
    pub fn connect_url(&self) -> String {
        format!(
            "{}{}",
            self.hub_url.trim_end_matches('/'),
            crate::protocol::NODE_CONNECT_PATH
        )
    }
}

/// Whether this machine stays connected to the hub with no agent running. Off by default: the daemon (which holds the connection) exits a short
/// while after the last agent ends, so nothing runs in the background without a window open. The person turns it on with
/// `claudecord settings keep-running on` (saved in `settings.json`); `CLAUDECORD_KEEP_RUNNING=1` or `0` overrides it for one run.
pub fn keep_running(dir: &Path) -> bool {
    if let Ok(v) = std::env::var("CLAUDECORD_KEEP_RUNNING") {
        return matches!(v.as_str(), "1" | "true" | "on" | "yes");
    }
    std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["keep_running"].as_bool())
        .unwrap_or(false)
}

/// Saves the keep-running setting.
pub fn set_keep_running(dir: &Path, on: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::json!({ "keep_running": on }).to_string(),
    )
}

/// The folders this machine has started agents in, by project, most recently used first. Older saves held one folder per project; those are read too.
pub fn read_projects(dir: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let Some(v) = std::fs::read_to_string(dir.join("projects.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return BTreeMap::new();
    };
    let Some(map) = v.as_object() else {
        return BTreeMap::new();
    };
    map.iter()
        .map(|(k, v)| {
            let list = match v {
                serde_json::Value::String(s) => vec![PathBuf::from(s)],
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(PathBuf::from))
                    .collect(),
                _ => vec![],
            };
            (k.clone(), list)
        })
        .collect()
}

/// The project a folder was last started for on this machine, if any.
pub fn project_of_folder(dir: &Path, folder: &Path) -> Option<String> {
    read_projects(dir)
        .into_iter()
        .find(|(_, folders)| folders.iter().any(|f| f == folder))
        .map(|(p, _)| p)
}
