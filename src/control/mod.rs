//! The control plane: the one small database (`control.db`) that knows WHO exists, before any team's own data is touched. It holds
//! accounts (one per Discord person, and each account is one tenant with its own hub data), dashboard sessions, the codes a machine
//! uses to ask a person to approve it, and the tokens of approved machines. A team's conversation never lives here; that is each
//! tenant's own file (see `registry`). Keeping "who is this" apart from "what did they say" is what lets a million accounts share one
//! front door without one ever being able to reach another's data.
//!
//! Only hashes of secrets are stored: a copy of this file does not let anyone sign in or connect a machine.

pub mod registry;
pub mod seal;

use crate::security::codes::{format_code, normalize_code};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

use crate::sync::Lock;

/// A person who has signed in with Discord. `id` is also the tenant id: the name of their folder and the owner of their machines.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub discord_id: String,
    pub name: String,
}

/// A startup command an account saved on the hub: any shell line (setup steps and the launch), and which agent program it starts, so the
/// screen reader knows how to tell idle from busy from a question. It runs on a machine only after that machine allowed it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Command {
    pub command: String,
    pub program: String,
}

/// The most commands an account may save, and the longest line.
pub const MAX_COMMANDS: usize = 50;
pub const MAX_COMMAND_CHARS: usize = 2000;

impl Command {
    /// Whether this is a command that may be saved under `name`, and if not, why.
    pub fn problem(&self, name: &str) -> Option<&'static str> {
        if !crate::protocol::is_slug(name) {
            Some(
                "a name is letters, digits, dots, dashes and underscores (up to 50), starting with a letter or digit",
            )
        } else if self.command.trim().is_empty() {
            Some("the command is empty")
        } else if self.command.chars().count() > MAX_COMMAND_CHARS {
            Some("the command is too long (2000 characters at most)")
        } else if self.command.contains('\0') {
            Some("the command has a character that cannot be run")
        } else if !["claude", "codex", "agy"].contains(&self.program.as_str()) {
            Some("the program is claude, codex or agy")
        } else {
            None
        }
    }
}

/// What a machine asking to be approved is told while it waits.
#[derive(Debug, PartialEq)]
pub enum Poll {
    /// Nobody has approved yet.
    Pending,
    /// A person approved it: this is the machine's token, given once and never stored in the clear.
    Approved {
        token: String,
        tenant: String,
        /// The name the person gave the machine when approving it, which is the name the hub knows it by.
        node: String,
    },
    /// The code ran out of time, was refused, or the token was already handed over.
    Gone,
}

/// A request from a machine, as the approval page shows it to the person.
#[derive(Debug, PartialEq)]
pub struct Pending {
    pub user_code: String,
    pub node_hint: String,
}

/// How long a person has to approve a machine.
pub const DEVICE_CODE_MS: i64 = 10 * 60 * 1000;
/// How long a dashboard sign-in lasts.
pub const SESSION_MS: i64 = 14 * 24 * 3600 * 1000;
/// Most machine names one account may have, so one account cannot fill the table.
pub const MAX_MACHINES: usize = 200;

/// The letters a typed code uses: no 0/O, 1/I/L, so a code read aloud or copied by eye is not misread.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// How long a replaced refresh token keeps working.
pub const REFRESH_GRACE_MS: i64 = 60_000;

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system has a random source");
    b
}

fn random_hex(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes * 2);
    let mut left = bytes;
    while left > 0 {
        let chunk = left.min(32);
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf[..chunk]).expect("the system has a random source");
        out.extend(buf[..chunk].iter().map(|b| format!("{b:02x}")));
        left -= chunk;
    }
    out
}

/// A code a person types: 8 letters from the readable alphabet, shown as `ABCD-EFGH`.
fn new_user_code() -> String {
    let raw: String = random_bytes::<8>()
        .iter()
        // 31 symbols and 256 values: the small bias is harmless for a code that expires in ten minutes and is rate limited.
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect();
    format_code(&raw)
}

/// The form a secret is stored in: its SHA-256 in hex.
pub fn hash_secret(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The control database.
pub struct Control {
    conn: Mutex<Connection>,
}

impl Control {
    /// Opens (making it if needed) the control database at a path.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// An in-memory control database, for tests.
    pub fn open_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS accounts (
                 id TEXT PRIMARY KEY, discord_id TEXT NOT NULL UNIQUE, name TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions (
                 hash TEXT PRIMARY KEY, account TEXT NOT NULL, expires INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS sessions_account ON sessions (account);
             CREATE TABLE IF NOT EXISTS device_codes (
                 device_hash TEXT PRIMARY KEY, user_code TEXT NOT NULL UNIQUE, node_hint TEXT NOT NULL,
                 expires INTEGER NOT NULL, account TEXT, node TEXT, state TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS machine_tokens (
                 hash TEXT PRIMARY KEY, tenant TEXT NOT NULL, node TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS machine_tokens_tenant ON machine_tokens (tenant, node);
             CREATE TABLE IF NOT EXISTS bots (
                 id TEXT PRIMARY KEY, tenant TEXT NOT NULL, app_id TEXT NOT NULL, name TEXT NOT NULL,
                 sealed TEXT NOT NULL, created INTEGER NOT NULL, UNIQUE (tenant, app_id));
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS project_targets (
                 tenant TEXT NOT NULL, project TEXT NOT NULL, bot TEXT NOT NULL, guild TEXT NOT NULL, channel TEXT NOT NULL,
                 guild_name TEXT NOT NULL DEFAULT '', channel_name TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (tenant, project));",
        )?;
        // Added after the first release: an existing database gets the column once, a new one already has it.
        let has_commands = conn
            .prepare("SELECT 1 FROM pragma_table_info('accounts') WHERE name = 'commands'")?
            .exists([])?;
        if !has_commands {
            conn.execute(
                "ALTER TABLE accounts ADD COLUMN commands TEXT NOT NULL DEFAULT '{}'",
                [],
            )?;
        }
        // Refresh tokens are replaced each time they are used; the old one keeps working for a minute, so a reply lost on the way does not lock a
        // machine out. Added after the first release, like the column above.
        let has_retire = conn
            .prepare("SELECT 1 FROM pragma_table_info('machine_tokens') WHERE name = 'retire'")?
            .exists([])?;
        if !has_retire {
            conn.execute("ALTER TABLE machine_tokens ADD COLUMN retire INTEGER", [])?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ----- accounts -----

    /// The account for a Discord person, made on first sign-in. The name is refreshed each time, because people rename themselves.
    pub fn sign_in_discord(
        &self,
        discord_id: &str,
        name: &str,
        now: i64,
    ) -> rusqlite::Result<Account> {
        let c = self.conn.locked();
        let existing: Option<String> = c
            .query_row(
                "SELECT id FROM accounts WHERE discord_id = ?1",
                params![discord_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                c.execute(
                    "UPDATE accounts SET name = ?2 WHERE id = ?1",
                    params![id, name],
                )?;
                id
            }
            None => {
                let id = format!("t{}", random_hex(8));
                c.execute(
                    "INSERT INTO accounts (id, discord_id, name, created) VALUES (?1, ?2, ?3, ?4)",
                    params![id, discord_id, name, now],
                )?;
                id
            }
        };
        Ok(Account {
            id,
            discord_id: discord_id.into(),
            name: name.into(),
        })
    }

    /// An account by id.
    /// The startup commands an account saved, by name.
    pub fn commands(
        &self,
        tenant: &str,
    ) -> rusqlite::Result<std::collections::BTreeMap<String, Command>> {
        let text: Option<String> = self
            .conn
            .locked()
            .query_row(
                "SELECT commands FROM accounts WHERE id = ?1",
                params![tenant],
                |r| r.get(0),
            )
            .optional()?;
        Ok(text
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    /// Saves (or replaces) one startup command of an account. Err says why it was refused.
    pub fn set_command(&self, tenant: &str, name: &str, cmd: Command) -> Result<(), String> {
        if let Some(why) = cmd.problem(name) {
            return Err(why.into());
        }
        let mut all = self.commands(tenant).map_err(|e| e.to_string())?;
        if !all.contains_key(name) && all.len() >= MAX_COMMANDS {
            return Err(format!(
                "an account can save {MAX_COMMANDS} commands at most"
            ));
        }
        all.insert(name.to_string(), cmd);
        self.write_commands(tenant, &all)
    }

    /// Removes one saved command. False if the account had none by that name.
    pub fn remove_command(&self, tenant: &str, name: &str) -> Result<bool, String> {
        let mut all = self.commands(tenant).map_err(|e| e.to_string())?;
        if all.remove(name).is_none() {
            return Ok(false);
        }
        self.write_commands(tenant, &all)?;
        Ok(true)
    }

    fn write_commands(
        &self,
        tenant: &str,
        all: &std::collections::BTreeMap<String, Command>,
    ) -> Result<(), String> {
        self.conn
            .locked()
            .execute(
                "UPDATE accounts SET commands = ?2 WHERE id = ?1",
                params![tenant, serde_json::to_string(all).expect("plain data")],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn account(&self, id: &str) -> rusqlite::Result<Option<Account>> {
        self.conn
            .locked()
            .query_row(
                "SELECT id, discord_id, name FROM accounts WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Account {
                        id: r.get(0)?,
                        discord_id: r.get(1)?,
                        name: r.get(2)?,
                    })
                },
            )
            .optional()
    }

    // ----- dashboard sessions -----

    /// Starts a dashboard session for an account. Returns the secret for the cookie; only its hash is kept.
    pub fn create_session(&self, account: &str, now: i64) -> rusqlite::Result<String> {
        let sid = random_hex(24);
        let c = self.conn.locked();
        // Old sessions are cleared as new ones are made, so the table never needs a separate cleaner.
        c.execute("DELETE FROM sessions WHERE expires <= ?1", params![now])?;
        c.execute(
            "INSERT INTO sessions (hash, account, expires) VALUES (?1, ?2, ?3)",
            params![hash_secret(&sid), account, now + SESSION_MS],
        )?;
        Ok(sid)
    }

    /// Who a session cookie belongs to, if it is a live session.
    pub fn session(&self, sid: &str, now: i64) -> rusqlite::Result<Option<Account>> {
        let id: Option<String> = self
            .conn
            .locked()
            .query_row(
                "SELECT account FROM sessions WHERE hash = ?1 AND expires > ?2",
                params![hash_secret(sid), now],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.account(&id),
            None => Ok(None),
        }
    }

    /// Ends a session now.
    pub fn end_session(&self, sid: &str) -> rusqlite::Result<()> {
        self.conn.locked().execute(
            "DELETE FROM sessions WHERE hash = ?1",
            params![hash_secret(sid)],
        )?;
        Ok(())
    }

    // ----- machines asking to join (device code) -----

    /// A machine asks to be approved. Returns `(device_code, user_code)`: the first is the machine's own secret for polling, the
    /// second is what the person sees and types or clicks. `node_hint` is the name the machine suggests for itself.
    pub fn device_start(&self, node_hint: &str, now: i64) -> rusqlite::Result<(String, String)> {
        let device = random_hex(24);
        let c = self.conn.locked();
        c.execute("DELETE FROM device_codes WHERE expires <= ?1", params![now])?;
        // A clash of 8-letter codes is a one-in-a-trillion; retry rather than reason about it.
        for _ in 0..5 {
            let user = new_user_code();
            let r = c.execute(
                "INSERT INTO device_codes (device_hash, user_code, node_hint, expires, state) VALUES (?1, ?2, ?3, ?4, 'pending')",
                params![hash_secret(&device), normalize_code(&user), node_hint, now + DEVICE_CODE_MS],
            );
            match r {
                Ok(_) => return Ok((device, user)),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation => {}
                Err(e) => return Err(e),
            }
        }
        Err(rusqlite::Error::StatementChangedRows(0))
    }

    /// What the approval page shows for a code a person typed: the machine's suggested name. None if the code is wrong, old or used.
    pub fn device_lookup(&self, user_code: &str, now: i64) -> rusqlite::Result<Option<Pending>> {
        let code = normalize_code(user_code);
        self.conn
            .locked()
            .query_row(
                "SELECT user_code, node_hint FROM device_codes WHERE user_code = ?1 AND expires > ?2 AND state = 'pending'",
                params![code, now],
                |r| {
                    Ok(Pending {
                        user_code: format_code(&r.get::<_, String>(0)?),
                        node_hint: r.get(1)?,
                    })
                },
            )
            .optional()
    }

    /// A signed-in person approves the machine, naming it. The first approval wins; a second finds the code already used.
    /// Returns false if the code is wrong, old or used, or the account already has too many machines.
    pub fn device_approve(
        &self,
        user_code: &str,
        account: &str,
        node: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let c = self.conn.locked();
        let have: i64 = c.query_row(
            "SELECT COUNT(DISTINCT node) FROM machine_tokens WHERE tenant = ?1",
            params![account],
            |r| r.get(0),
        )?;
        if have as usize >= MAX_MACHINES {
            return Ok(false);
        }
        let n = c.execute(
            "UPDATE device_codes SET state = 'approved', account = ?2, node = ?3 WHERE user_code = ?1 AND expires > ?4 AND state = 'pending'",
            params![normalize_code(user_code), account, node, now],
        )?;
        Ok(n == 1)
    }

    /// A person refuses a machine. The machine is told its code is gone.
    pub fn device_deny(&self, user_code: &str) -> rusqlite::Result<()> {
        self.conn.locked().execute(
            "DELETE FROM device_codes WHERE user_code = ?1 AND state = 'pending'",
            params![normalize_code(user_code)],
        )?;
        Ok(())
    }

    /// The machine asks whether it has been approved. Once approved, the token is made right here, handed over once, and the code
    /// is removed, so the secret never sits in the database in the clear and a replayed poll gets nothing.
    pub fn device_poll(&self, device_code: &str, now: i64) -> rusqlite::Result<Poll> {
        let h = hash_secret(device_code);
        let c = self.conn.locked();
        let row: Option<(String, i64, Option<String>, Option<String>)> = c
            .query_row(
                "SELECT state, expires, account, node FROM device_codes WHERE device_hash = ?1",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((state, expires, account, node)) = row else {
            return Ok(Poll::Gone);
        };
        if expires <= now {
            c.execute(
                "DELETE FROM device_codes WHERE device_hash = ?1",
                params![h],
            )?;
            return Ok(Poll::Gone);
        }
        if state != "approved" {
            return Ok(Poll::Pending);
        }
        let (Some(tenant), Some(node)) = (account, node) else {
            return Ok(Poll::Gone);
        };
        // The prefix is what lets the screen-picture and file scrubbers recognise a machine token wherever it shows up.
        let token = format!("ccn1.{}", random_hex(32));
        // Taking the code and making the token are one step: either the machine gets a working token or nothing changed.
        let tx = c.unchecked_transaction()?;
        let gone = tx.execute(
            "DELETE FROM device_codes WHERE device_hash = ?1 AND state = 'approved'",
            params![h],
        )?;
        if gone != 1 {
            return Ok(Poll::Gone);
        }
        tx.execute(
            "INSERT INTO machine_tokens (hash, tenant, node, created) VALUES (?1, ?2, ?3, ?4)",
            params![hash_secret(&token), tenant, node, now],
        )?;
        tx.commit()?;
        Ok(Poll::Approved {
            token,
            tenant,
            node,
        })
    }

    // ----- machine tokens -----

    /// Which tenant and machine a token belongs to, if it is a real, unrevoked token.
    pub fn machine_for_token(&self, token: &str) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .locked()
            .query_row(
                "SELECT tenant, node FROM machine_tokens WHERE hash = ?1 AND (retire IS NULL OR retire > ?2)",
                params![hash_secret(token), crate::now_ms()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Whether the account still has this machine (a revoked machine's access tokens stop working at once, not when they run out).
    pub fn machine_exists(&self, tenant: &str, node: &str) -> rusqlite::Result<bool> {
        self.conn.locked().query_row(
            "SELECT EXISTS(SELECT 1 FROM machine_tokens WHERE tenant = ?1 AND node = ?2 AND (retire IS NULL OR retire > ?3))",
            params![tenant, node, crate::now_ms()],
            |r| r.get(0),
        )
    }

    /// Exchanges a refresh token for a new one: the machine's login carries on with the new token, and the old one works for `REFRESH_GRACE_MS`
    /// more, then is gone. None if the token is not a live one.
    pub fn rotate_machine_token(
        &self,
        token: &str,
        now: i64,
    ) -> rusqlite::Result<Option<(String, String, String)>> {
        let c = self.conn.locked();
        let tx = c.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM machine_tokens WHERE retire IS NOT NULL AND retire <= ?1",
            params![now],
        )?;
        let found: Option<(String, String)> = tx
            .query_row(
                "SELECT tenant, node FROM machine_tokens WHERE hash = ?1 AND (retire IS NULL OR retire > ?2)",
                params![hash_secret(token), now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((tenant, node)) = found else {
            return Ok(None);
        };
        let fresh = format!("ccn1.{}", random_hex(32));
        tx.execute(
            "INSERT INTO machine_tokens (hash, tenant, node, created) VALUES (?1, ?2, ?3, ?4)",
            params![hash_secret(&fresh), tenant, node, now],
        )?;
        tx.execute(
            "UPDATE machine_tokens SET retire = ?1 WHERE hash = ?2 AND retire IS NULL",
            params![now + REFRESH_GRACE_MS, hash_secret(token)],
        )?;
        tx.commit()?;
        Ok(Some((tenant, node, fresh)))
    }

    /// The machines an account has approved, with when each was first approved.
    pub fn machines(&self, tenant: &str) -> rusqlite::Result<Vec<(String, i64)>> {
        let c = self.conn.locked();
        let mut s = c.prepare(
            "SELECT node, MIN(created) FROM machine_tokens WHERE tenant = ?1 GROUP BY node ORDER BY node",
        )?;
        s.query_map(params![tenant], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Cancels every token a machine has. It can no longer connect.
    pub fn revoke_machine(&self, tenant: &str, node: &str) -> rusqlite::Result<usize> {
        self.conn.locked().execute(
            "DELETE FROM machine_tokens WHERE tenant = ?1 AND node = ?2",
            params![tenant, node],
        )
    }

    // ----- bots (sealed tokens) -----

    /// Saves a bot token, already sealed (see `seal`). Saving the same bot again replaces its token.
    pub fn save_bot(
        &self,
        tenant: &str,
        app_id: &str,
        name: &str,
        sealed: &str,
        now: i64,
    ) -> rusqlite::Result<String> {
        let c = self.conn.locked();
        let existing: Option<String> = c
            .query_row(
                "SELECT id FROM bots WHERE tenant = ?1 AND app_id = ?2",
                params![tenant, app_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                c.execute(
                    "UPDATE bots SET sealed = ?2, name = ?3 WHERE id = ?1",
                    params![id, sealed, name],
                )?;
                Ok(id)
            }
            None => {
                let id = format!("b{}", random_hex(6));
                c.execute(
                    "INSERT INTO bots (id, tenant, app_id, name, sealed, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, tenant, app_id, name, sealed, now],
                )?;
                Ok(id)
            }
        }
    }

    /// An account's bots as (id, application id, name). The token itself is never part of a listing.
    pub fn bots(&self, tenant: &str) -> rusqlite::Result<Vec<(String, String, String)>> {
        let c = self.conn.locked();
        let mut s =
            c.prepare("SELECT id, app_id, name FROM bots WHERE tenant = ?1 ORDER BY created")?;
        s.query_map(params![tenant], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    /// The sealed token of one of an account's bots. Asking with another account's id finds nothing.
    pub fn sealed_bot(&self, tenant: &str, bot: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .locked()
            .query_row(
                "SELECT sealed FROM bots WHERE tenant = ?1 AND id = ?2",
                params![tenant, bot],
                |r| r.get(0),
            )
            .optional()
    }

    /// Removes a bot and the places it was set to post to.
    pub fn delete_bot(&self, tenant: &str, bot: &str) -> rusqlite::Result<bool> {
        let c = self.conn.locked();
        c.execute(
            "DELETE FROM project_targets WHERE tenant = ?1 AND bot = ?2",
            params![tenant, bot],
        )?;
        Ok(c.execute(
            "DELETE FROM bots WHERE tenant = ?1 AND id = ?2",
            params![tenant, bot],
        )? == 1)
    }

    // ----- where each project posts -----

    /// This service's own identity, made once and kept. It is written into the Discord channels this service uses, so that a second service (a
    /// test copy, say) using the same bot can tell a channel is taken, and leaves it alone.
    pub fn hub_id(&self) -> rusqlite::Result<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(v) = conn.query_row("SELECT value FROM meta WHERE key = 'hub_id'", [], |r| {
            r.get::<_, String>(0)
        }) {
            return Ok(v);
        }
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('hub_id', ?1)",
            params![id],
        )?;
        conn.query_row("SELECT value FROM meta WHERE key = 'hub_id'", [], |r| {
            r.get(0)
        })
    }

    /// Sets where a project lives in Discord: which bot, server and channel (and their names, for showing).
    pub fn set_target(&self, tenant: &str, p: &Placement) -> rusqlite::Result<()> {
        self.conn.locked().execute(
            "INSERT INTO project_targets (tenant, project, bot, guild, channel, guild_name, channel_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (tenant, project) DO UPDATE SET bot = ?3, guild = ?4, channel = ?5, guild_name = ?6, channel_name = ?7",
            params![tenant, p.project, p.bot, p.guild, p.channel, p.guild_name, p.channel_name],
        )?;
        Ok(())
    }

    /// Where a project posts.
    pub fn target(&self, tenant: &str, project: &str) -> rusqlite::Result<Option<Placement>> {
        self.conn
            .locked()
            .query_row(
                "SELECT project, bot, guild, channel, guild_name, channel_name FROM project_targets WHERE tenant = ?1 AND project = ?2",
                params![tenant, project],
                placement_row,
            )
            .optional()
    }

    /// Every project of an account that has a place to post.
    pub fn targets(&self, tenant: &str) -> rusqlite::Result<Vec<Placement>> {
        let c = self.conn.locked();
        let mut s = c.prepare(
            "SELECT project, bot, guild, channel, guild_name, channel_name FROM project_targets WHERE tenant = ?1 ORDER BY project",
        )?;
        s.query_map(params![tenant], placement_row)?.collect()
    }
}

/// Where a project posts in Discord.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub project: String,
    pub bot: String,
    pub guild: String,
    pub channel: String,
    pub guild_name: String,
    pub channel_name: String,
}

fn placement_row(r: &rusqlite::Row) -> rusqlite::Result<Placement> {
    Ok(Placement {
        project: r.get(0)?,
        bot: r.get(1)?,
        guild: r.get(2)?,
        channel: r.get(3)?,
        guild_name: r.get(4)?,
        channel_name: r.get(5)?,
    })
}
