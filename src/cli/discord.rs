//! `claudecord discord`: connect the hub to your Discord server. You create a bot in Discord, give the hub its token and
//! your server's id, and invite the bot with the link this prints. The token is kept in a private file in the hub's data
//! folder, never in the environment or on a command line that a shell would remember.

use crate::discord::{bridge::BridgeConfig, perms};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Args)]
pub struct DiscordArgs {
    /// Where the hub keeps its files.
    #[arg(long, global = true, default_value = "claudecord-hub")]
    pub data: PathBuf,
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Save the bot's token and your server's id.
    Set {
        /// Your Discord server's id (turn on Developer Mode in Discord, right-click the server, Copy Server ID).
        #[arg(long)]
        guild: String,
        /// File holding the bot token. If omitted, it is read from standard input.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Print the link that invites the bot to your server with exactly the permissions it needs.
    Invite,
    /// Show the saved settings (never the token).
    Show,
}

/// What is saved.
#[derive(Serialize, Deserialize)]
struct Saved {
    token: String,
    guild: String,
}

/// Runs a discord command.
pub fn run(a: DiscordArgs) -> Result<(), String> {
    std::fs::create_dir_all(&a.data).map_err(|e| e.to_string())?;
    let path = a.data.join("discord.json");
    match a.action {
        Action::Set { guild, token_file } => {
            let mut token = String::new();
            match token_file {
                Some(f) => {
                    token = std::fs::read_to_string(&f)
                        .map_err(|e| format!("cannot read {}: {e}", f.display()))?
                }
                None => {
                    eprintln!("Paste the bot token, then press Enter and Ctrl-D:");
                    std::io::stdin()
                        .read_to_string(&mut token)
                        .map_err(|e| e.to_string())?;
                }
            }
            let token = token.trim().to_string();
            if token.is_empty() {
                return Err("the token is empty".into());
            }
            if perms::app_id_from_token(&token).is_none() {
                return Err("that does not look like a Discord bot token".into());
            }
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&Saved { token, guild }).expect("plain data"),
            )
            .map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())?;
            }
            println!(
                "saved (readable by you only). Next: claudecord discord invite, then start the hub."
            );
            Ok(())
        }
        Action::Invite => {
            let s = load(&path)?;
            let app = perms::app_id_from_token(&s.token).ok_or("the saved token is not valid")?;
            println!("{}", perms::invite_url(&app));
            eprintln!(
                "Open it, choose your server, and approve. In the bot's settings also turn on the Message Content intent."
            );
            Ok(())
        }
        Action::Show => {
            let s = load(&path)?;
            println!(
                "guild  {}\ntoken  (hidden, bot id {})",
                s.guild,
                perms::app_id_from_token(&s.token).unwrap_or_default()
            );
            Ok(())
        }
    }
}

fn load(path: &Path) -> Result<Saved, String> {
    let t = std::fs::read_to_string(path).map_err(|_| {
        format!(
            "{} not found: run claudecord discord set first",
            path.display()
        )
    })?;
    serde_json::from_str(&t).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

/// The bridge settings for a hub's data folder, if Discord has been set up there.
pub fn bridge_config(data: &Path) -> Result<Option<BridgeConfig>, String> {
    let path = data.join("discord.json");
    if !path.exists() {
        return Ok(None);
    }
    let s = load(&path)?;
    Ok(Some(BridgeConfig {
        api_base: "https://discord.com/api/v10".into(),
        token: s.token,
        guild: s.guild,
        gateway_url: None,
        db_path: data.join("hub.db"),
        owners: vec![],
        backoff_max: Duration::from_secs(60),
    }))
}
