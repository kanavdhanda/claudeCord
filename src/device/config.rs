//! What a machine remembers between runs: the hub address, its token, and its name. Stored as a small JSON file in the
//! user's own folder, readable by that user only, because the token lets anyone who has it act as this machine.

use serde::{Deserialize, Serialize};
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
