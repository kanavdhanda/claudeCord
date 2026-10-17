//! `claudecord-hub storage`: where old history files go. Set up Oracle Cloud or Cloudflare R2 (or any S3-compatible bucket),
//! test that it works, and move everything from one provider to another. The bucket's keys are kept in the hub's data
//! folder, in a file only the owner can read, never in the environment or on a command line that a shell would remember.

use crate::store::Store;
use crate::store::bucket::Bucket;
use clap::{Args, Subcommand};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct StorageArgs {
    /// Where the hub keeps its files.
    #[arg(long, global = true, default_value = "claudecord-hub")]
    pub data: PathBuf,
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Use Oracle Cloud Object Storage (its S3-compatible door). Create a Customer Secret Key in your Oracle profile first.
    Oracle {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        region: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        key_id: String,
        /// File holding the secret key. If omitted, it is read from standard input.
        #[arg(long)]
        secret_file: Option<PathBuf>,
        /// Write the settings here instead of making them the active ones (used before `move`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Use Cloudflare R2.
    R2 {
        #[arg(long)]
        account: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        key_id: String,
        #[arg(long)]
        secret_file: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check the saved bucket works: write, read back and delete a small object.
    Test,
    /// Copy every history file to the bucket described in FILE, check each copy, then switch over to it.
    Move { file: PathBuf },
    /// Copy the live database to the bucket now (the hub also does this every few hours).
    Backup,
    /// Bring the database back from the bucket onto this machine (for a host that lost its disk).
    Restore {
        /// Which backup (default: the newest).
        #[arg(long, default_value = "backup/hub-latest.db.gz")]
        key: String,
        /// Replace a database that is already here.
        #[arg(long)]
        force: bool,
    },
    /// Show where history goes now (never the secret).
    Show,
}

/// Runs a storage command.
pub fn run(a: StorageArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let active = a.data.join("storage.json");
    match a.action {
        Action::Oracle {
            namespace,
            region,
            bucket,
            key_id,
            secret_file,
            out,
        } => {
            let b = Bucket::oracle(&namespace, &region, &bucket, &key_id, &secret(secret_file)?);
            save(&b, out.as_deref().unwrap_or(&active))
        }
        Action::R2 {
            account,
            bucket,
            key_id,
            secret_file,
            out,
        } => {
            let b = Bucket::r2(&account, &bucket, &key_id, &secret(secret_file)?);
            save(&b, out.as_deref().unwrap_or(&active))
        }
        Action::Test => {
            let b = load(&active)?;
            let key = format!("claudecord-test-{}", crate::now_ms());
            b.put(&key, b"ok")
                .map_err(|e| format!("could not write: {e}"))?;
            let back = b
                .get(&key)
                .map_err(|e| format!("could not read back: {e}"))?;
            b.delete(&key)
                .map_err(|e| format!("could not delete: {e}"))?;
            if back != b"ok" {
                return Err("read back something different from what was written".into());
            }
            println!(
                "ok: wrote, read back and deleted a test object in {} ({})",
                b.bucket, b.endpoint
            );
            Ok(())
        }
        Action::Move { file } => {
            let from = load(&active)?;
            let to = load(&file)?;
            let store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
                .map_err(|e| e.to_string())?;
            let n = store
                .migrate_segments(&from, &to)
                .map_err(|e| format!("copy failed, nothing was switched: {e}"))?;
            std::fs::copy(&active, a.data.join("storage.previous.json"))
                .map_err(|e| e.to_string())?;
            save(&to, &active)?;
            println!(
                "copied and verified {n} file(s). Now using {}. The old bucket was left untouched (settings kept in storage.previous.json); delete its objects yourself when you are happy.",
                to.endpoint
            );
            Ok(())
        }
        Action::Backup => {
            let mut store = Store::open(&a.data.join("hub.db"), Some(&a.data.join("history")))
                .map_err(|e| e.to_string())?;
            store.set_bucket(Some(load(&active)?));
            let key = store
                .backup_to_bucket(crate::now_ms() / 1000)
                .map_err(|e| format!("backup failed: {e}"))?;
            println!("backed up to {key} and backup/hub-latest.db.gz");
            Ok(())
        }
        Action::Restore { key, force } => {
            Store::restore_from_bucket(&load(&active)?, &key, &a.data.join("hub.db"), force)
                .map_err(|e| e.to_string())?;
            println!("restored {key} into {}", a.data.join("hub.db").display());
            Ok(())
        }
        Action::Show => {
            let b = load(&active)?;
            println!(
                "endpoint {}\nregion   {}\nbucket   {}\nkey id   {}\nsecret   (hidden)",
                b.endpoint, b.region, b.bucket, b.key_id
            );
            Ok(())
        }
    }
}

/// The secret key, from a file or from standard input. Trailing whitespace is dropped.
fn secret(file: Option<PathBuf>) -> Result<String, String> {
    let mut s = String::new();
    match file {
        Some(f) => {
            s = std::fs::read_to_string(&f)
                .map_err(|e| format!("cannot read {}: {e}", f.display()))?
        }
        None => {
            eprintln!("Paste the secret key, then press Enter and Ctrl-D:");
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| e.to_string())?;
        }
    }
    let s = s.trim().to_string();
    if s.is_empty() {
        Err("the secret key is empty".into())
    } else {
        Ok(s)
    }
}

/// Saves bucket settings privately.
fn save(b: &Bucket, path: &Path) -> Result<(), String> {
    std::fs::write(path, serde_json::to_string_pretty(b).expect("plain data"))
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    println!(
        "saved {} (readable by you only). Next: claudecord-hub storage test",
        path.display()
    );
    Ok(())
}

/// Reads saved bucket settings.
pub fn load(path: &Path) -> Result<Bucket, String> {
    let text = std::fs::read_to_string(path).map_err(|_| {
        format!(
            "{} not found: set a provider up first (claudecord-hub storage oracle ...)",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))
}
