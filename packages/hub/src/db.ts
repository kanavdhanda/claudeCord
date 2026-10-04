import { createRequire } from "node:module";
import type { DatabaseSync as DatabaseSyncType } from "node:sqlite";
import { createHash, randomBytes } from "node:crypto";
import { chmodSync, existsSync } from "node:fs";

// Loaded via require so bundlers and test runners that do not know node:sqlite leave it alone.
const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");

export interface ProjectRow {
  name: string;
  channel_id: string;
  webhook_id: string | null;
  webhook_token: string | null;
  status_message_id: string | null;
  /** Name reserved for the lead agent when the project was created from the dashboard. */
  lead_name?: string | null;
  created?: number | null;
}

export interface LoginRow {
  code_hash: string;
  poll_hash: string;
  expires: number;
  state: "pending" | "approved" | "denied";
  device: string;
  folder: string | null;
  trusted: number;
  claimed: number;
  decision: string | null;
}

export interface AgentRow {
  agent_id: string;
  name: string;
  project: string;
  node_name: string;
  adapter: string;
  model: string | null;
  role: string | null;
  is_lead: number;
}

export type TaskState = "assigned" | "accepted" | "done";

export interface TaskRow {
  /** Display id such as T3, unique within a project. */
  id: string;
  project: string;
  num: number;
  from_agent: string;
  to_agent: string;
  text: string;
  state: TaskState;
  summary: string | null;
  created: number;
  updated: number;
}

export class Db {
  private db: DatabaseSyncType;
  // Agents are read on every routed message, so they are served from memory and written through to SQLite.
  private byId = new Map<string, AgentRow>();
  private byProject = new Map<string, Map<string, AgentRow>>();

  constructor(path: string) {
    this.db = new DatabaseSync(path);
    if (path !== ":memory:") {
      // The hub and the claudecord-hub command can both open this file, so wait for a lock instead of failing, and let
      // readers and the writer work at the same time.
      this.db.exec("PRAGMA busy_timeout = 3000; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;");
    }
    // The database holds webhook tokens and task text. Keep it private to this user.
    if (path !== ":memory:" && existsSync(path)) chmodSync(path, 0o600);
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS tokens (
        hash TEXT PRIMARY KEY,
        node_name TEXT NOT NULL UNIQUE,
        revoked INTEGER NOT NULL DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS projects (
        name TEXT PRIMARY KEY,
        channel_id TEXT NOT NULL,
        webhook_id TEXT,
        webhook_token TEXT,
        status_message_id TEXT
      );
      CREATE TABLE IF NOT EXISTS agents (
        agent_id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        project TEXT NOT NULL,
        node_name TEXT NOT NULL,
        adapter TEXT NOT NULL,
        model TEXT,
        role TEXT,
        is_lead INTEGER NOT NULL DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS login_sessions (
        code_hash TEXT PRIMARY KEY,
        poll_hash TEXT NOT NULL UNIQUE,
        expires INTEGER NOT NULL,
        state TEXT NOT NULL,
        device TEXT NOT NULL,
        folder TEXT,
        trusted INTEGER NOT NULL,
        claimed INTEGER NOT NULL DEFAULT 0,
        decision TEXT
      );
      CREATE TABLE IF NOT EXISTS kv (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS pair_codes (
        hash TEXT PRIMARY KEY,
        expires INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS login_tokens (
        hash TEXT PRIMARY KEY,
        expires INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS sessions (
        hash TEXT PRIMARY KEY,
        expires INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS tasks (
        project TEXT NOT NULL,
        num INTEGER NOT NULL,
        from_agent TEXT NOT NULL,
        to_agent TEXT NOT NULL,
        text TEXT NOT NULL,
        state TEXT NOT NULL,
        summary TEXT,
        created INTEGER NOT NULL,
        updated INTEGER NOT NULL,
        PRIMARY KEY (project, num)
      );
      -- Lookups by project, device and expiry are on every hot path, so they are indexed.
      CREATE INDEX IF NOT EXISTS idx_agents_project ON agents(project);
      CREATE INDEX IF NOT EXISTS idx_agents_node ON agents(node_name);
      CREATE INDEX IF NOT EXISTS idx_tasks_project_state ON tasks(project, state);
      CREATE INDEX IF NOT EXISTS idx_tokens_node ON tokens(node_name);
      CREATE INDEX IF NOT EXISTS idx_pair_expires ON pair_codes(expires);
      CREATE INDEX IF NOT EXISTS idx_login_expires ON login_tokens(expires);
      CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires);
      CREATE INDEX IF NOT EXISTS idx_login_expires ON login_sessions(expires);
    `);
    // Older databases predate these two columns.
    const cols = (this.db.prepare("PRAGMA table_info(projects)").all() as { name: string }[]).map((c) => c.name);
    if (!cols.includes("lead_name")) this.db.exec("ALTER TABLE projects ADD COLUMN lead_name TEXT");
    if (!cols.includes("created")) this.db.exec("ALTER TABLE projects ADD COLUMN created INTEGER");
    for (const t of this.db.prepare("SELECT * FROM tasks").all() as unknown as Omit<TaskRow, "id">[]) this.cacheTask(t);
    for (const a of this.db.prepare("SELECT * FROM agents").all() as unknown as AgentRow[]) this.cache(a);
  }

  private tasks = new Map<string, Map<number, TaskRow>>();

  private cacheTask(t: Omit<TaskRow, "id">): TaskRow {
    const row: TaskRow = { ...t, id: `T${t.num}` };
    let m = this.tasks.get(t.project);
    if (!m) this.tasks.set(t.project, (m = new Map()));
    m.set(t.num, row);
    return row;
  }

  createTask(project: string, from: string, to: string, text: string): TaskRow {
    const num = Math.max(0, ...(this.tasks.get(project)?.keys() ?? [])) + 1;
    const now = Date.now();
    this.db
      .prepare(
        "INSERT INTO tasks(project,num,from_agent,to_agent,text,state,summary,created,updated) VALUES(?,?,?,?,?,?,?,?,?)",
      )
      .run(project, num, from, to, text, "assigned", null, now, now);
    return this.cacheTask({
      project,
      num,
      from_agent: from,
      to_agent: to,
      text,
      state: "assigned",
      summary: null,
      created: now,
      updated: now,
    });
  }

  getTask(project: string, id: string): TaskRow | undefined {
    const num = Number(id.replace(/^T/i, ""));
    return this.tasks.get(project)?.get(num);
  }

  setTaskState(project: string, id: string, state: TaskState, summary?: string): TaskRow | undefined {
    const t = this.getTask(project, id);
    if (!t) return undefined;
    t.state = state;
    t.updated = Date.now();
    if (summary !== undefined) t.summary = summary;
    this.db
      .prepare("UPDATE tasks SET state=?, summary=?, updated=? WHERE project=? AND num=?")
      .run(t.state, t.summary, t.updated, project, t.num);
    return t;
  }

  tasksOfProject(project: string): TaskRow[] {
    return [...(this.tasks.get(project)?.values() ?? [])].sort((a, b) => a.num - b.num);
  }

  private cache(a: AgentRow): void {
    this.byId.set(a.agent_id, a);
    let m = this.byProject.get(a.project);
    if (!m) this.byProject.set(a.project, (m = new Map()));
    m.set(a.agent_id, a);
  }

  private uncache(id: string): void {
    const a = this.byId.get(id);
    if (!a) return;
    this.byId.delete(id);
    const m = this.byProject.get(a.project);
    m?.delete(id);
    if (m && !m.size) this.byProject.delete(a.project);
  }

  getAgent(id: string): AgentRow | undefined {
    return this.byId.get(id);
  }

  createToken(nodeName: string): string {
    const token = `ccn1.${randomBytes(24).toString("base64url")}`;
    this.db
      .prepare(
        "INSERT INTO tokens(hash,node_name) VALUES(?,?) ON CONFLICT(node_name) DO UPDATE SET hash=excluded.hash, revoked=0",
      )
      .run(hash(token), nodeName);
    return token;
  }

  /** Stores a secret as its hash with an expiry. Only the hash is ever kept. */
  putSecret(table: "pair_codes" | "login_tokens" | "sessions", secret: string, ttlMs: number): void {
    this.db.prepare(`INSERT INTO ${table}(hash,expires) VALUES(?,?)`).run(hash(secret), Date.now() + ttlMs);
  }

  /** Deletes the secret and returns true if it existed and had not expired. Single use. */
  takeSecret(table: "pair_codes" | "login_tokens", secret: string): boolean {
    const h = hash(secret);
    const row = this.db.prepare(`SELECT expires FROM ${table} WHERE hash=?`).get(h) as { expires: number } | undefined;
    if (!row) return false;
    this.db.prepare(`DELETE FROM ${table} WHERE hash=?`).run(h);
    return row.expires > Date.now();
  }

  hasSecret(table: "sessions", secret: string): boolean {
    const row = this.db.prepare(`SELECT expires FROM ${table} WHERE hash=?`).get(hash(secret)) as
      { expires: number } | undefined;
    return !!row && row.expires > Date.now();
  }

  deleteSecret(table: "sessions", secret: string): void {
    this.db.prepare(`DELETE FROM ${table} WHERE hash=?`).run(hash(secret));
  }

  pruneExpired(): void {
    const now = Date.now();
    for (const t of ["pair_codes", "login_tokens", "sessions"])
      this.db.prepare(`DELETE FROM ${t} WHERE expires < ?`).run(now);
  }

  deviceExists(nodeName: string): boolean {
    return !!this.db.prepare("SELECT 1 FROM tokens WHERE node_name=?").get(nodeName);
  }

  listDevices(): { node_name: string; revoked: number }[] {
    return this.db.prepare("SELECT node_name, revoked FROM tokens ORDER BY node_name").all() as unknown as {
      node_name: string;
      revoked: number;
    }[];
  }

  revokeToken(nodeName: string): boolean {
    return Number(this.db.prepare("UPDATE tokens SET revoked=1 WHERE node_name=?").run(nodeName).changes) > 0;
  }

  nodeForToken(token: string): string | null {
    const row = this.db.prepare("SELECT node_name FROM tokens WHERE hash=? AND revoked=0").get(hash(token)) as
      { node_name: string } | undefined;
    return row?.node_name ?? null;
  }

  getProject(name: string): ProjectRow | undefined {
    return this.db.prepare("SELECT * FROM projects WHERE name=?").get(name) as ProjectRow | undefined;
  }

  projectByChannel(channelId: string): ProjectRow | undefined {
    return this.db.prepare("SELECT * FROM projects WHERE channel_id=?").get(channelId) as ProjectRow | undefined;
  }

  listProjects(): ProjectRow[] {
    return this.db.prepare("SELECT * FROM projects ORDER BY name").all() as unknown as ProjectRow[];
  }

  /** Records who made a project and what its lead is called. Keeps the first values if called again. */
  setProjectMeta(name: string, leadName: string | null): void {
    this.db
      .prepare("UPDATE projects SET lead_name = COALESCE(lead_name, ?), created = COALESCE(created, ?) WHERE name = ?")
      .run(leadName, Date.now(), name);
  }

  kvGet(key: string): string | undefined {
    return (this.db.prepare("SELECT value FROM kv WHERE key=?").get(key) as { value: string } | undefined)?.value;
  }

  kvSet(key: string, value: string): void {
    this.db
      .prepare("INSERT INTO kv(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
      .run(key, value);
  }

  // Login sessions: a command line asks to be signed in, and the owner's browser approves it.

  createLoginSession(
    code: string,
    poll: string,
    o: { device: string; folder?: string; trusted: boolean; ttlMs: number },
  ): void {
    this.db
      .prepare(
        "INSERT INTO login_sessions(code_hash,poll_hash,expires,state,device,folder,trusted) VALUES(?,?,?,?,?,?,?)",
      )
      .run(hash(code), hash(poll), Date.now() + o.ttlMs, "pending", o.device, o.folder ?? null, o.trusted ? 1 : 0);
  }

  private live(row: LoginRow | undefined): LoginRow | undefined {
    return row && row.expires > Date.now() ? row : undefined;
  }

  loginByCode(code: string): LoginRow | undefined {
    return this.live(
      this.db.prepare("SELECT * FROM login_sessions WHERE code_hash=?").get(hash(code)) as LoginRow | undefined,
    );
  }

  loginByPoll(poll: string): LoginRow | undefined {
    return this.live(
      this.db.prepare("SELECT * FROM login_sessions WHERE poll_hash=?").get(hash(poll)) as LoginRow | undefined,
    );
  }

  /** True only the first time, so a sign-in link mints a browser session once. */
  claimLogin(code: string): boolean {
    return (
      Number(
        this.db
          .prepare("UPDATE login_sessions SET claimed=1 WHERE code_hash=? AND claimed=0 AND expires>?")
          .run(hash(code), Date.now()).changes,
      ) > 0
    );
  }

  /** Moves a pending login to approved or denied. Returns false if it was not pending. */
  decideLogin(code: string, state: "approved" | "denied", decision: object | null): boolean {
    return (
      Number(
        this.db
          .prepare("UPDATE login_sessions SET state=?, decision=? WHERE code_hash=? AND state='pending' AND expires>?")
          .run(state, decision ? JSON.stringify(decision) : null, hash(code), Date.now()).changes,
      ) > 0
    );
  }

  deleteLoginByPoll(poll: string): void {
    this.db.prepare("DELETE FROM login_sessions WHERE poll_hash=?").run(hash(poll));
  }

  saveProject(p: ProjectRow): void {
    this.db
      .prepare(
        `INSERT INTO projects(name,channel_id,webhook_id,webhook_token,status_message_id)
         VALUES(?,?,?,?,?)
         ON CONFLICT(name) DO UPDATE SET channel_id=excluded.channel_id, webhook_id=excluded.webhook_id,
           webhook_token=excluded.webhook_token, status_message_id=excluded.status_message_id`,
      )
      .run(p.name, p.channel_id, p.webhook_id, p.webhook_token, p.status_message_id);
  }

  upsertAgent(a: AgentRow): void {
    this.db
      .prepare(
        `INSERT INTO agents(agent_id,name,project,node_name,adapter,model,role,is_lead)
         VALUES(?,?,?,?,?,?,?,?)
         ON CONFLICT(agent_id) DO UPDATE SET name=excluded.name, project=excluded.project,
           node_name=excluded.node_name, adapter=excluded.adapter, model=excluded.model, role=excluded.role`,
      )
      .run(a.agent_id, a.name, a.project, a.node_name, a.adapter, a.model, a.role, a.is_lead);
    const prev = this.byId.get(a.agent_id);
    this.uncache(a.agent_id);
    this.cache({ ...a, is_lead: prev?.is_lead ?? a.is_lead });
  }

  removeAgent(agentId: string): void {
    this.db.prepare("DELETE FROM agents WHERE agent_id=?").run(agentId);
    this.uncache(agentId);
  }

  agentsOfProject(project: string): AgentRow[] {
    return [...(this.byProject.get(project)?.values() ?? [])];
  }

  allAgents(): AgentRow[] {
    return [...this.byId.values()];
  }

  setLead(project: string, agentId: string): void {
    this.db.prepare("UPDATE agents SET is_lead=0 WHERE project=?").run(project);
    this.db.prepare("UPDATE agents SET is_lead=1 WHERE agent_id=?").run(agentId);
    for (const a of this.byProject.get(project)?.values() ?? []) a.is_lead = a.agent_id === agentId ? 1 : 0;
  }

  clearAgentsOfNode(nodeName: string): void {
    this.db.prepare("DELETE FROM agents WHERE node_name=?").run(nodeName);
    for (const a of this.allAgents()) if (a.node_name === nodeName) this.uncache(a.agent_id);
  }

  agentsOfNode(nodeName: string): AgentRow[] {
    return this.allAgents().filter((a) => a.node_name === nodeName);
  }
}

function hash(s: string): string {
  return createHash("sha256").update(s).digest("hex");
}
