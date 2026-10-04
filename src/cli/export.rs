//! `claudecord export`: writes the conversation history as an Obsidian vault. Run it once, or with `--watch` to keep the
//! vault up to date while the hub runs. It reads the hub's data folder (the database and the compressed history files, from
//! the bucket if that is where they went) and never changes it.

use crate::export::obsidian;
use crate::store::Store;
use clap::Args;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct ExportArgs {
    /// Where the hub keeps its files.
    #[arg(long, default_value = "claudecord-hub")]
    pub data: PathBuf,
    /// The vault folder to write into (open this folder in Obsidian).
    #[arg(long, default_value = "claudecord-vault")]
    pub out: PathBuf,
    /// Only this project.
    #[arg(long)]
    pub project: Option<String>,
    /// Keep running and refresh the vault every this many seconds.
    #[arg(long)]
    pub watch: Option<u64>,
}

/// Exports once, or repeatedly with `--watch`.
pub fn run(a: ExportArgs) -> Result<(), String> {
    loop {
        let n = export_once(&a)?;
        println!("vault {} updated ({n} file(s) written)", a.out.display());
        match a.watch {
            Some(secs) => std::thread::sleep(Duration::from_secs(secs.max(1))),
            None => return Ok(()),
        }
    }
}

/// One export pass. Returns how many files changed.
pub(crate) fn export_once(a: &ExportArgs) -> Result<usize, String> {
    let mut store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
        .map_err(|e| e.to_string())?;
    if a.data.join("storage.json").exists() {
        store.set_bucket(Some(super::storage::load(&a.data.join("storage.json"))?));
    }
    let projects = store.projects().map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for p in projects
        .iter()
        .filter(|p| a.project.as_ref().is_none_or(|w| w == *p))
    {
        rows.extend(store.history_all(p).map_err(|e| e.to_string())?);
    }
    obsidian::write_vault(&a.out, &obsidian::render(&rows)).map_err(|e| e.to_string())
}
