export type SessionStatus = "online" | "offline" | "waiting" | "idle";

export interface Project {
  channel_id: string;
  project_path: string;
  guild_id: string;
  auto_approve: number; // 0 or 1
  repo_url: string | null;
  mute_peers: number; // 0 or 1 - ignore messages from other agents
  plan_first: number | null; // null = use global PLAN_FIRST default
  discord_channel_id: string | null; // real channel; channel_id is the agent key
  persona: string | null;
  avatar_url: string | null;
  role_id: string | null;
  terminal_pid: number | null;
  terminal_since: number | null;
  is_primary: number; // 0 or 1
  created_at: string;
}

export interface Session {
  id: string;
  channel_id: string;
  session_id: string | null; // Claude Agent SDK session ID
  status: SessionStatus;
  last_activity: string | null;
  created_at: string;
}

export interface UsageRow {
  user_id: string;
  channel_id: string;
  turns: number;
  cost_usd: number;
  duration_ms: number;
}

export interface AuditRow {
  id: number;
  ts: string;
  guild_id: string | null;
  channel_id: string | null;
  user_id: string | null;
  action: string;
  detail: string | null;
}
