import { createRequire } from "node:module";
import type { DatabaseSync as DatabaseSyncType } from "node:sqlite";
import { createHash, randomBytes } from "node:crypto";

// Loaded via require so bundlers and test runners that do not know node:sqlite leave it alone.
const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");

export interface ProjectRow {
  name: string;
  channel_id: string;
  webhook_id: string | null;
  webhook_token: string | null;
  status_message_id: string | null;
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

export class Db {
  private db: DatabaseSyncType;

  constructor(path: string) {
    this.db = new DatabaseSync(path);
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
    `);
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

  revokeToken(nodeName: string): boolean {
    return Number(this.db.prepare("UPDATE tokens SET revoked=1 WHERE node_name=?").run(nodeName).changes) > 0;
  }

  nodeForToken(token: string): string | null {
    const row = this.db
      .prepare("SELECT node_name FROM tokens WHERE hash=? AND revoked=0")
      .get(hash(token)) as { node_name: string } | undefined;
    return row?.node_name ?? null;
  }

  getProject(name: string): ProjectRow | undefined {
    return this.db.prepare("SELECT * FROM projects WHERE name=?").get(name) as ProjectRow | undefined;
  }

  projectByChannel(channelId: string): ProjectRow | undefined {
    return this.db.prepare("SELECT * FROM projects WHERE channel_id=?").get(channelId) as ProjectRow | undefined;
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
  }

  removeAgent(agentId: string): void {
    this.db.prepare("DELETE FROM agents WHERE agent_id=?").run(agentId);
  }

  agentsOfProject(project: string): AgentRow[] {
    return this.db.prepare("SELECT * FROM agents WHERE project=?").all(project) as unknown as AgentRow[];
  }

  allAgents(): AgentRow[] {
    return this.db.prepare("SELECT * FROM agents").all() as unknown as AgentRow[];
  }

  setLead(project: string, agentId: string): void {
    this.db.prepare("UPDATE agents SET is_lead=0 WHERE project=?").run(project);
    this.db.prepare("UPDATE agents SET is_lead=1 WHERE agent_id=?").run(agentId);
  }

  clearAgentsOfNode(nodeName: string): void {
    this.db.prepare("DELETE FROM agents WHERE node_name=?").run(nodeName);
  }
}

function hash(s: string): string {
  return createHash("sha256").update(s).digest("hex");
}
