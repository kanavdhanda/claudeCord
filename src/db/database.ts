import Database from "better-sqlite3";
import type { AuditRow, Project, Session, SessionStatus, UsageRow } from "./types.js";

import { dbPath } from "../utils/paths.js";

let db: Database.Database;

export function initDatabase(): void {
  db = new Database(dbPath());
  db.pragma("journal_mode = WAL");
  db.pragma("foreign_keys = ON");

  db.exec(`
    CREATE TABLE IF NOT EXISTS projects (
      channel_id TEXT PRIMARY KEY,
      project_path TEXT NOT NULL,
      guild_id TEXT NOT NULL,
      auto_approve INTEGER DEFAULT 0,
      created_at TEXT DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS sessions (
      id TEXT PRIMARY KEY,
      channel_id TEXT REFERENCES projects(channel_id) ON DELETE CASCADE,
      session_id TEXT,
      status TEXT DEFAULT 'offline',
      last_activity TEXT,
      created_at TEXT DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS usage (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      ts TEXT DEFAULT (datetime('now')),
      guild_id TEXT,
      channel_id TEXT,
      user_id TEXT,
      source TEXT DEFAULT 'human',
      cost_usd REAL DEFAULT 0,
      duration_ms INTEGER DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS settings (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS webhooks (
      channel_id TEXT PRIMARY KEY,
      webhook_id TEXT NOT NULL,
      token TEXT NOT NULL
    );

    -- Agents seen in a channel (local and remote), learned from their hello messages
    CREATE TABLE IF NOT EXISTS directory (
      webhook_id TEXT NOT NULL,
      persona TEXT NOT NULL,
      channel_id TEXT NOT NULL,
      role_id TEXT NOT NULL,
      host TEXT NOT NULL,
      seen_at TEXT DEFAULT (datetime('now')),
      PRIMARY KEY (webhook_id, persona)
    );

    CREATE TABLE IF NOT EXISTS audit (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      ts TEXT DEFAULT (datetime('now')),
      guild_id TEXT,
      channel_id TEXT,
      user_id TEXT,
      action TEXT NOT NULL,
      detail TEXT
    );

    CREATE TABLE IF NOT EXISTS live_board (
      guild_id TEXT PRIMARY KEY,
      channel_id TEXT NOT NULL,
      message_id TEXT NOT NULL
    );
  `);

  // Additive migrations for databases created by earlier versions
  for (const ddl of [
    "ALTER TABLE projects ADD COLUMN repo_url TEXT",
    "ALTER TABLE projects ADD COLUMN mute_peers INTEGER DEFAULT 0",
    "ALTER TABLE projects ADD COLUMN plan_first INTEGER",
    "ALTER TABLE projects ADD COLUMN discord_channel_id TEXT",
    "ALTER TABLE projects ADD COLUMN persona TEXT",
    "ALTER TABLE projects ADD COLUMN avatar_url TEXT",
    "ALTER TABLE projects ADD COLUMN role_id TEXT",
    "ALTER TABLE projects ADD COLUMN terminal_pid INTEGER",
    "ALTER TABLE projects ADD COLUMN terminal_since INTEGER",
    "ALTER TABLE projects ADD COLUMN is_primary INTEGER DEFAULT 0",
  ]) {
    try {
      db.exec(ddl);
    } catch {
      // column already exists
    }
  }
}

export function getDb(): Database.Database {
  return db;
}

// Project queries
export function registerProject(
  channelId: string,
  projectPath: string,
  guildId: string,
  repoUrl: string | null = null,
): void {
  const stmt = db.prepare(`
    INSERT OR REPLACE INTO projects (channel_id, project_path, guild_id, repo_url)
    VALUES (?, ?, ?, ?)
  `);
  stmt.run(channelId, projectPath, guildId, repoUrl);
}

export function getEveryProject(): Project[] {
  return db.prepare("SELECT * FROM projects").all() as Project[];
}

export function setMutePeers(channelId: string, mute: boolean): void {
  db.prepare("UPDATE projects SET mute_peers = ? WHERE channel_id = ?").run(mute ? 1 : 0, channelId);
}

export function setPlanFirst(channelId: string, planFirst: boolean | null): void {
  db.prepare("UPDATE projects SET plan_first = ? WHERE channel_id = ?").run(
    planFirst === null ? null : planFirst ? 1 : 0,
    channelId,
  );
}

export function unregisterProject(channelId: string): void {
  db.prepare("DELETE FROM sessions WHERE channel_id = ?").run(channelId);
  db.prepare("DELETE FROM projects WHERE channel_id = ?").run(channelId);
}

export function getProject(channelId: string): Project | undefined {
  return db
    .prepare("SELECT * FROM projects WHERE channel_id = ?")
    .get(channelId) as Project | undefined;
}

export function getAllProjects(guildId: string): Project[] {
  return db
    .prepare("SELECT * FROM projects WHERE guild_id = ?")
    .all(guildId) as Project[];
}

export function getPrimaryAgent(guildId: string): Project | undefined {
  return db
    .prepare("SELECT * FROM projects WHERE guild_id = ? AND is_primary = 1 LIMIT 1")
    .get(guildId) as Project | undefined;
}

export function setPrimaryAgent(channelId: string, guildId: string): void {
  db.prepare("UPDATE projects SET is_primary = 0 WHERE guild_id = ?").run(guildId);
  db.prepare("UPDATE projects SET is_primary = 1 WHERE channel_id = ?").run(channelId);
}

export function setAutoApprove(
  channelId: string,
  autoApprove: boolean,
): void {
  db.prepare("UPDATE projects SET auto_approve = ? WHERE channel_id = ?").run(
    autoApprove ? 1 : 0,
    channelId,
  );
}

// Session queries
export function upsertSession(
  id: string,
  channelId: string,
  sessionId: string | null,
  status: SessionStatus,
): void {
  const stmt = db.prepare(`
    INSERT OR REPLACE INTO sessions (id, channel_id, session_id, status, last_activity)
    VALUES (?, ?, ?, ?, datetime('now'))
  `);
  stmt.run(id, channelId, sessionId, status);
}

export function getSession(channelId: string): Session | undefined {
  return db
    .prepare(
      "SELECT * FROM sessions WHERE channel_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .get(channelId) as Session | undefined;
}

export function updateSessionStatus(
  channelId: string,
  status: SessionStatus,
): void {
  db.prepare(
    "UPDATE sessions SET status = ?, last_activity = datetime('now') WHERE channel_id = ?",
  ).run(status, channelId);
}

export function getAllSessions(guildId: string): (Session & { project_path: string })[] {
  return db
    .prepare(`
      SELECT s.*, p.project_path FROM sessions s
      JOIN projects p ON s.channel_id = p.channel_id
      WHERE p.guild_id = ?
    `)
    .all(guildId) as (Session & { project_path: string })[];
}

// Usage tracker
export function recordUsage(entry: {
  guildId: string | null;
  channelId: string;
  userId: string;
  source: "human" | "peer";
  costUsd: number;
  durationMs: number;
}): void {
  db.prepare(
    "INSERT INTO usage (guild_id, channel_id, user_id, source, cost_usd, duration_ms) VALUES (?, ?, ?, ?, ?, ?)",
  ).run(entry.guildId, entry.channelId, entry.userId, entry.source, entry.costUsd, entry.durationMs);
}

/** Usage grouped by user and channel since a SQLite datetime string (UTC). */
export function getUsageSummary(guildId: string, sinceUtc: string): UsageRow[] {
  return db
    .prepare(`
      SELECT user_id, channel_id, COUNT(*) AS turns,
             COALESCE(SUM(cost_usd), 0) AS cost_usd,
             COALESCE(SUM(duration_ms), 0) AS duration_ms
      FROM usage WHERE guild_id = ? AND ts >= ?
      GROUP BY user_id, channel_id ORDER BY cost_usd DESC
    `)
    .all(guildId, sinceUtc) as UsageRow[];
}

// Audit log
export function recordAudit(entry: {
  guildId?: string | null;
  channelId?: string | null;
  userId?: string | null;
  action: string;
  detail?: string;
}): void {
  db.prepare(
    "INSERT INTO audit (guild_id, channel_id, user_id, action, detail) VALUES (?, ?, ?, ?, ?)",
  ).run(entry.guildId ?? null, entry.channelId ?? null, entry.userId ?? null, entry.action, entry.detail ?? null);
}

export function getRecentAudit(guildId: string, limit = 15): AuditRow[] {
  return db
    .prepare("SELECT * FROM audit WHERE guild_id = ? ORDER BY id DESC LIMIT ?")
    .all(guildId, limit) as AuditRow[];
}

// --- Agents (personas) -------------------------------------------------------

export interface AgentRegistration {
  key: string;
  discordChannelId: string;
  projectPath: string;
  guildId: string;
  persona: string;
  repoUrl?: string | null;
  avatarUrl?: string | null;
  roleId?: string | null;
}

export function registerAgent(a: AgentRegistration): void {
  db.prepare(`
    INSERT OR REPLACE INTO projects
      (channel_id, project_path, guild_id, repo_url, discord_channel_id, persona, avatar_url, role_id)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
  `).run(a.key, a.projectPath, a.guildId, a.repoUrl ?? null, a.discordChannelId, a.persona, a.avatarUrl ?? null, a.roleId ?? null);
}

/** Agents this device runs in a Discord channel. */
export function getAgentsInChannel(discordChannelId: string): Project[] {
  return db
    .prepare("SELECT * FROM projects WHERE COALESCE(discord_channel_id, channel_id) = ? ORDER BY created_at, channel_id")
    .all(discordChannelId) as Project[];
}

export function getAgentByPersona(guildId: string, persona: string): Project | undefined {
  return db
    .prepare("SELECT * FROM projects WHERE guild_id = ? AND lower(persona) = lower(?)")
    .get(guildId, persona) as Project | undefined;
}

export function getAgentByPersonaAnywhere(persona: string): Project[] {
  return db.prepare("SELECT * FROM projects WHERE lower(persona) = lower(?)").all(persona) as Project[];
}

export function setAgentRole(key: string, roleId: string): void {
  db.prepare("UPDATE projects SET role_id = ? WHERE channel_id = ?").run(roleId, key);
}

export function getWebhookRecord(channelId: string): { webhook_id: string; token: string } | undefined {
  return db.prepare("SELECT webhook_id, token FROM webhooks WHERE channel_id = ?").get(channelId) as
    | { webhook_id: string; token: string }
    | undefined;
}

export function saveWebhookRecord(channelId: string, webhookId: string, token: string): void {
  db.prepare("INSERT OR REPLACE INTO webhooks (channel_id, webhook_id, token) VALUES (?, ?, ?)").run(channelId, webhookId, token);
}

export function getOwnWebhookIds(): string[] {
  return (db.prepare("SELECT webhook_id FROM webhooks").all() as { webhook_id: string }[]).map((r) => r.webhook_id);
}

// --- Directory of agents seen in channels -------------------------------------

export interface DirectoryEntry {
  webhook_id: string;
  persona: string;
  channel_id: string;
  role_id: string;
  host: string;
}

export function upsertDirectory(e: DirectoryEntry): void {
  db.prepare(`
    INSERT OR REPLACE INTO directory (webhook_id, persona, channel_id, role_id, host, seen_at)
    VALUES (?, ?, ?, ?, ?, datetime('now'))
  `).run(e.webhook_id, e.persona, e.channel_id, e.role_id, e.host);
}

export function getDirectoryForChannel(channelId: string): DirectoryEntry[] {
  return db.prepare("SELECT * FROM directory WHERE channel_id = ?").all(channelId) as DirectoryEntry[];
}

export function removeDirectoryEntry(webhookId: string, persona: string): void {
  db.prepare("DELETE FROM directory WHERE webhook_id = ? AND persona = ?").run(webhookId, persona);
}

// --- Terminal takeover -------------------------------------------------------

export function setTerminalHold(key: string, pid: number | null): void {
  db.prepare("UPDATE projects SET terminal_pid = ?, terminal_since = ? WHERE channel_id = ?").run(
    pid,
    pid === null ? null : Date.now(),
    key,
  );
}

// --- Device settings ---------------------------------------------------------

export function getSetting(key: string): string | undefined {
  return (db.prepare("SELECT value FROM settings WHERE key = ?").get(key) as { value: string } | undefined)?.value;
}

export function setSetting(key: string, value: string): void {
  db.prepare("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)").run(key, value);
}

/**
 * When several devices share one bot, exactly one answers slash commands that don't name an
 * agent or a host. A device that never joined someone else's workspace is primary.
 */
export function isPrimaryDevice(): boolean {
  return getSetting("primary") !== "0";
}

// --- Live board --------------------------------------------------------------

export interface LiveBoard {
  guild_id: string;
  channel_id: string;
  message_id: string;
}

export function getLiveBoard(guildId: string): LiveBoard | undefined {
  return db.prepare("SELECT * FROM live_board WHERE guild_id = ?").get(guildId) as LiveBoard | undefined;
}

export function setLiveBoard(guildId: string, channelId: string, messageId: string): void {
  db.prepare("INSERT OR REPLACE INTO live_board (guild_id, channel_id, message_id) VALUES (?, ?, ?)").run(guildId, channelId, messageId);
}

export function clearLiveBoard(guildId: string): void {
  db.prepare("DELETE FROM live_board WHERE guild_id = ?").run(guildId);
}
