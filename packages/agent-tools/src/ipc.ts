import { createConnection } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

export type AgentRequest =
  | { op: "say"; agentId: string; text: string; thread?: string }
  | { op: "ask"; agentId: string; question: string; options?: string[]; thread?: string }
  | { op: "report"; agentId: string; title: string; summary: string; artifacts?: string[] }
  | { op: "assign"; agentId: string; to: string; task: string; thread?: string }
  | { op: "taskdone"; agentId: string; taskId: string; summary: string }
  | { op: "send"; agentId: string; path: string; to?: string; caption?: string; thread?: string };

export type ControlRequest =
  | { op: "ping" }
  | { op: "up"; cwd: string; project: string; name?: string; adapter?: string; model?: string; role?: string }
  | { op: "down"; agent?: string; project?: string }
  | { op: "ls" }
  | { op: "shutdown" };

export type DaemonRequest = AgentRequest | ControlRequest;
export type DaemonResponse = { ok: true; data?: unknown } | { ok: false; error: string };

export function meshDir(): string {
  return process.env.CLAUDECORD_HOME ?? join(homedir(), ".claude-mesh");
}

export function sockPath(): string {
  return process.env.CLAUDECORD_SOCK ?? join(meshDir(), "node.sock");
}

export function callDaemon(req: DaemonRequest, timeoutMs?: number): Promise<DaemonResponse> {
  return new Promise((resolve, reject) => {
    const sock = createConnection(sockPath());
    let buf = "";
    const timer = timeoutMs ? setTimeout(() => { sock.destroy(); reject(new Error("timeout")); }, timeoutMs) : undefined;
    sock.on("connect", () => sock.write(JSON.stringify(req) + "\n"));
    sock.on("data", (d) => {
      buf += d.toString();
      const i = buf.indexOf("\n");
      if (i >= 0) {
        clearTimeout(timer);
        sock.end();
        try {
          resolve(JSON.parse(buf.slice(0, i)) as DaemonResponse);
        } catch (e) {
          reject(e);
        }
      }
    });
    sock.on("error", (e) => { clearTimeout(timer); reject(e); });
    sock.on("close", () => { clearTimeout(timer); if (!buf.includes("\n")) reject(new Error("daemon closed connection")); });
  });
}

export function currentAgentId(): string {
  const id = process.env.CLAUDECORD_AGENT_ID;
  if (!id) throw new Error("CLAUDECORD_AGENT_ID is not set. This command must run inside a claudecord agent.");
  return id;
}
