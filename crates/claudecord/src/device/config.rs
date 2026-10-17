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
    settings_value(dir)["keep_running"]
        .as_bool()
        .unwrap_or(false)
}

fn settings_value(dir: &Path) -> serde_json::Value {
    std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_setting(dir: &Path, key: &str, on: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut v = settings_value(dir);
    v[key] = serde_json::Value::Bool(on);
    std::fs::write(dir.join("settings.json"), v.to_string())
}

/// Saves the keep-running setting.
pub fn set_keep_running(dir: &Path, on: bool) -> std::io::Result<()> {
    write_setting(dir, "keep_running", on)
}

/// Whether this machine runs the startup commands the hub sends with a spawn (the ones saved on the dashboard). Off by default: a command is
/// a line of shell, and a machine runs one only after its owner said so here, with `claudecord settings custom-commands on`.
/// `CLAUDECORD_CUSTOM_COMMANDS=1` or `0` overrides it for one run.
pub fn custom_commands_allowed(dir: &Path) -> bool {
    if let Ok(v) = std::env::var("CLAUDECORD_CUSTOM_COMMANDS") {
        return matches!(v.as_str(), "1" | "true" | "on" | "yes");
    }
    settings_value(dir)["custom_commands"]
        .as_bool()
        .unwrap_or(false)
}

/// Saves the custom-commands setting.
pub fn set_custom_commands(dir: &Path, on: bool) -> std::io::Result<()> {
    write_setting(dir, "custom_commands", on)
}

/// The folders this machine has started agents in, by project, most recently used first. Older saves held one folder per project; those are read too.
/// A folder may be listed under several projects (the same code can serve more than one team); `read_last` says which was used last.
pub fn read_projects(dir: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let Some(map) = read_projects_json(dir) else {
        return BTreeMap::new();
    };
    map.iter()
        // Keys that start with an underscore are bookkeeping, never a project (a project's name starts with a letter or digit).
        .filter(|(k, _)| !k.starts_with('_'))
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

fn read_projects_json(dir: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = std::fs::read_to_string(dir.join("projects.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .as_object()
        .cloned()
}

/// For each folder, the projects it was started for, the most recent first.
pub fn read_last(dir: &Path) -> BTreeMap<String, Vec<String>> {
    read_projects_json(dir)
        .and_then(|m| m.get("_last").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Saves the folders of each project and which project each folder was last used for.
pub fn write_projects(
    dir: &Path,
    folders: &BTreeMap<String, Vec<PathBuf>>,
    last: &BTreeMap<String, Vec<String>>,
) -> std::io::Result<()> {
    let mut v = serde_json::to_value(folders).expect("plain data");
    v["_last"] = serde_json::to_value(last).expect("plain data");
    std::fs::write(dir.join("projects.json"), v.to_string())
}

/// The project a folder was last started for on this machine, if any.
pub fn project_of_folder(dir: &Path, folder: &Path) -> Option<String> {
    let folders = read_projects(dir);
    let has = |p: &String| {
        folders
            .get(p)
            .is_some_and(|l| l.iter().any(|f| f == folder))
    };
    read_last(dir)
        .get(folder.to_string_lossy().as_ref())
        .and_then(|l| l.iter().find(|p| has(p)).cloned())
        // Saved before recency was kept: the first project (by name) that lists the folder.
        .or_else(|| folders.keys().find(|p| has(p)).cloned())
}

/// The shell line to run for a command, as the program and arguments to start: the person's own shell, as a login shell, so what their
/// terminal has (a PATH set up by nvm, an activated environment) is there. Not available on Windows.
pub fn shell_argv(command: &str) -> Result<Vec<String>, String> {
    if cfg!(windows) {
        return Err(
            "saved startup commands need a Unix shell and are not available on Windows".into(),
        );
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty() && Path::new(s).is_file())
        .unwrap_or_else(|| "/bin/sh".into());
    Ok(vec![shell, "-lc".into(), command.to_string()])
}

#[cfg(test)]
mod setting_tests {
    use super::*;

    #[test]
    fn one_setting_does_not_erase_another_and_both_start_off() {
        let d = std::env::temp_dir().join(format!("cc-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(!keep_running(&d) && !custom_commands_allowed(&d));
        set_keep_running(&d, true).unwrap();
        set_custom_commands(&d, true).unwrap();
        assert!(keep_running(&d) && custom_commands_allowed(&d));
        set_keep_running(&d, false).unwrap();
        assert!(
            !keep_running(&d) && custom_commands_allowed(&d),
            "turning one off leaves the other as it was"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_runs_in_the_persons_login_shell() {
        let argv = shell_argv("echo hi && exit 3").unwrap();
        assert_eq!(&argv[1..], ["-lc", "echo hi && exit 3"]);
        assert!(std::path::Path::new(&argv[0]).is_file());
    }

    #[cfg(windows)]
    #[test]
    fn saved_commands_are_refused_on_windows_with_the_reason() {
        let why = shell_argv("echo hi").unwrap_err();
        assert!(why.contains("Windows"), "{why}");
    }
}
