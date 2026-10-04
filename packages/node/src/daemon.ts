import { createServer, type Server, type Socket } from "node:net";
import { chmodSync, existsSync, mkdirSync, unlinkSync, writeFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { meshDir, sockPath, type DaemonRequest, type DaemonResponse } from "@claudecord/agent-tools";
import {
  NODE_CONNECT_PATH,
  autoName,
  encode,
  parseHubFrame,
  AdapterId,
  AgentSpec as AgentSpecSchema,
  redact,
  type AgentSpec,
  type HubFrame,
  type NodeFrame,
} from "@claudecord/protocol";
import { getAdapter } from "./adapters/index.js";
import { AgentRuntime } from "./agent.js";
import {
  ensureMeshDir,
  loadNodeConfig,
  loadProjectConfig,
  loadProjectDirs,
  saveProjectDir,
  type NodeConfig,
  type Policy,
} from "./config.js";
import { secretEnvNames } from "./env.js";
import { FileReceiver, readChunks, resolveInside } from "./files.js";
import { Sampler } from "./sampler.js";
import { buildRules } from "./rules.js";
import * as tmux from "./tmux.js";

const VERSION = "0.1.0";

/** The MCP server ships next to the CLI in the published bundle. In a dev checkout it comes from the workspace package. */
export function mcpPath(): string {
  const sibling = fileURLToPath(new URL("./mcp.js", import.meta.url));
  return existsSync(sibling) ? sibling : fileURLToPath(import.meta.resolve("@claudecord/agent-tools/mcp"));
}

export function cliPath(): string {
  return fileURLToPath(new URL("./cli.js", import.meta.url));
}

interface SpawnOpts {
  cwd?: string;
  project: string;
  name?: string;
  adapter?: AdapterId;
  model?: string;
  role?: string;
  policy?: Policy;
}

export class Daemon {
  private cfg: NodeConfig;
  private ws?: WebSocket;
  private agents = new Map<string, AgentRuntime>();
  private asks = new Map<string, { agentId: string; resolve: (text: string) => void }>();
  private askSeq = 0;
  private backoff = 1000;
  private closing = false;
  private server?: Server;
  private files = new FileReceiver();
  private sampler = new Sampler();
  private chain: Promise<void> = Promise.resolve();

  /** `handleSignals: false` is for tests, where the process must not exit on SIGINT or SIGTERM. */
  constructor(private opts: { handleSignals?: boolean } = {}) {
    this.cfg = loadNodeConfig();
  }

  /** Stops every agent, drops the hub link and closes the local socket. */
  async close(): Promise<void> {
    this.closing = true;
    this.sampler.stop();
    for (const a of [...this.agents.values()]) await this.stopAgent(a.spec.agentId);
    this.ws?.close();
    await new Promise<void>((r) => (this.server ? this.server.close(() => r()) : r()));
    try {
      unlinkSync(sockPath());
    } catch {
      /* already gone */
    }
  }

  async start(): Promise<void> {
    if (process.platform === "win32") {
      throw new Error("The node daemon needs tmux, which is not available on native Windows. Run it inside WSL2.");
    }
    if (!(await tmux.hasTmux()))
      throw new Error("tmux is required but was not found on PATH. Install it with brew or apt.");
    ensureMeshDir();
    this.listen();
    this.connect();
    this.sampler.start();
    console.log(`claudecord node "${this.cfg.nodeName}" started`);
  }

  // Hub link

  private connect(): void {
    const base = this.cfg.hubUrl.replace(/\/$/, "").replace(/^http/, "ws");
    const ws = new WebSocket(`${base}${NODE_CONNECT_PATH}`, { headers: { Authorization: `Bearer ${this.cfg.token}` } });
    this.ws = ws;
    ws.on("open", () => {
      this.backoff = 1000;
      this.send({ t: "hello", nodeName: this.cfg.nodeName, version: VERSION });
      for (const a of this.agents.values()) this.send({ t: "agent.register", agent: a.spec, cwd: a.cwd });
      console.log("connected to hub");
    });
    ws.on("message", (d) => {
      const f = parseHubFrame(d.toString());
      // Handled in order so file chunks and answers are never reordered.
      if (f) this.chain = this.chain.then(() => this.onHub(f)).catch((e) => console.error("hub frame error", e));
    });
    ws.on("close", () => {
      if (this.closing) return;
      // Full jitter, so many nodes do not reconnect in lockstep after a hub restart.
      const delay = Math.round(this.backoff * (0.5 + Math.random()));
      console.log(`hub disconnected, retrying in ${delay}ms`);
      setTimeout(() => this.connect(), delay);
      this.backoff = Math.min(this.backoff * 2, 30_000);
    });
    ws.on("error", (e) => console.error("hub link error:", e.message));
  }

  private send(f: NodeFrame): boolean {
    if (this.ws?.readyState !== WebSocket.OPEN) return false;
    this.ws.send(encode(f));
    return true;
  }

  private async onHub(f: HubFrame): Promise<void> {
    switch (f.t) {
      case "welcome":
      case "error":
        if (f.t === "error") console.error("hub:", f.message);
        return;
      case "deliver":
        this.agents.get(f.agentId)?.enqueue({ from: f.from, text: f.text, thread: f.thread, msgId: f.msgId });
        return;
      case "answer": {
        const ask = this.asks.get(f.askId);
        if (ask) {
          this.asks.delete(f.askId);
          ask.resolve(f.text);
          return;
        }
        const ok = await this.agents.get(f.agentId)?.answerPrompt(f.askId, f.text);
        if (ok === false) {
          this.send({
            t: "agent.say",
            agentId: f.agentId,
            text: "I could not map that reply onto the prompt on screen. Reply with an option number.",
          });
        }
        return;
      }
      case "file.chunk": {
        const rt = this.agents.get(f.agentId);
        if (!rt) return;
        const r = await this.files.receive(rt.cwd, f);
        if (r) {
          const kb = Math.max(1, Math.round(r.bytes / 1024));
          const note = f.caption ? ` Note: ${f.caption}` : "";
          rt.enqueue({
            from: f.from,
            thread: f.thread,
            text: `sent you a file, saved at ${relative(rt.cwd, r.path)} (${kb} KB).${note}`,
          });
        }
        return;
      }
      case "spawn": {
        // Only folders this device registered itself. The hub never gets to choose a directory.
        const dir = loadProjectDirs()[f.agent.project];
        if (!dir) {
          this.send({
            t: "agent.say",
            agentId: f.agent.agentId,
            text: `No directory known for project ${f.agent.project} on this device.`,
          });
          return;
        }
        await this.spawnAgent({ ...f.agent, cwd: dir });
        return;
      }
      case "stop":
        await this.stopAgent(f.agentId);
        return;
      case "killall":
        for (const a of [...this.agents.values()])
          if (!f.project || a.spec.project === f.project) await this.stopAgent(a.spec.agentId);
        return;
      case "hold":
        for (const a of this.agents.values()) {
          if (f.agentId ? a.spec.agentId === f.agentId : !f.project || a.spec.project === f.project) a.setHeld(f.on);
        }
        return;
    }
  }

  // Agents

  async spawnAgent(o: SpawnOpts): Promise<AgentSpec> {
    const cwd = o.cwd ?? process.cwd();
    const pc = loadProjectConfig(cwd);
    const adapterId = (o.adapter ?? pc?.adapter ?? this.cfg.adapter) as AdapterId;
    const adapter = getAdapter(adapterId);
    const taken = new Set(
      [...this.agents.values()].filter((a) => a.spec.project === o.project).map((a) => a.spec.name),
    );
    const name = o.name ?? autoName(taken);
    const spec: AgentSpec = {
      agentId: `${o.project}/${name}`,
      name,
      project: o.project,
      adapter: adapterId,
      model: o.model ?? pc?.model ?? this.cfg.model,
      role: o.role ?? pc?.role ?? this.cfg.role,
    };
    const checked = AgentSpecSchema.safeParse(spec);
    if (!checked.success)
      throw new Error(
        `invalid agent: ${checked.error.issues.map((i) => `${i.path.join(".") || "spec"} ${i.message}`).join("; ")}`,
      );
    if (this.agents.has(spec.agentId)) throw new Error(`agent ${name} already running`);
    const policy = o.policy ?? pc?.policy ?? this.cfg.policy;

    const useMcp = adapter.rulesVia === "flag";
    const rules = buildRules(spec, { shimCmd: `"${process.execPath}" "${cliPath()}"`, mcp: useMcp });
    let mcpConfigPath: string | undefined;
    if (useMcp) {
      const dir = join(meshDir(), "agents", spec.agentId.replace(/[^\w.-]+/g, "_"));
      mkdirSync(dir, { recursive: true });
      mcpConfigPath = join(dir, "mcp.json");
      const mcpBin = mcpPath();
      writeFileSync(
        mcpConfigPath,
        JSON.stringify({
          mcpServers: {
            claudecord: {
              command: process.execPath,
              args: [mcpBin],
              env: { CLAUDECORD_AGENT_ID: spec.agentId, CLAUDECORD_SOCK: sockPath() },
            },
          },
        }),
      );
    }

    const argv = adapter.argv({ spec, policy, rules, mcpConfigPath });
    // Agents do not inherit credentials from this shell. Includes variables the tmux server already carries.
    const scrub = (this.cfg.envPolicy ?? "scrub") === "scrub";
    const unset = scrub
      ? [
          ...new Set([
            ...secretEnvNames(process.env, adapterId),
            ...secretEnvNames(
              Object.fromEntries((await tmux.serverEnvNames()).map((n) => [n, process.env[n] ?? "x"])),
              adapterId,
            ),
          ]),
        ]
      : [];
    const paneId = await tmux.openPane({
      session: `claude-${o.project}`,
      cwd,
      title: `${name} (${adapterId})`,
      argv,
      unset,
      env: { CLAUDECORD_AGENT_ID: spec.agentId, CLAUDECORD_SOCK: sockPath() },
    });

    const rt = new AgentRuntime(
      spec,
      paneId,
      cwd,
      adapter,
      {
        status: (id, status, detail) => this.send({ t: "agent.status", agentId: id, status, detail }),
        ask: (id, askId, question, options) => this.send({ t: "agent.ask", agentId: id, askId, question, options }),
        limit: (id, info) => this.send({ t: "agent.limit", agentId: id, kind: info.kind, resetsAt: info.resetsAt }),
        accepted: (id, msgIds) => this.send({ t: "agent.accepted", agentId: id, msgIds }),
        gone: (id) => {
          this.agents.delete(id);
          this.send({ t: "agent.gone", agentId: id });
        },
      },
      useMcp ? undefined : rules,
    );
    this.agents.set(spec.agentId, rt);
    saveProjectDir(o.project, cwd);
    this.send({ t: "agent.register", agent: spec, cwd });
    this.sampler.add(rt);
    return spec;
  }

  async stopAgent(agentId: string): Promise<boolean> {
    const rt = this.agents.get(agentId);
    if (!rt) return false;
    this.agents.delete(agentId);
    this.sampler.remove(rt);
    await rt.stop();
    this.send({ t: "agent.gone", agentId });
    return true;
  }

  // Local IPC (cc CLI and agent tools)

  private listen(): void {
    const p = sockPath();
    if (existsSync(p)) unlinkSync(p);
    const server = createServer((sock) => this.onSocket(sock));
    this.server = server;
    // The socket can start agents and send files, so only this user may connect.
    server.listen(p, () => {
      if (process.platform !== "win32") chmodSync(p, 0o600);
    });
    if (this.opts.handleSignals === false) return;
    const cleanup = () => {
      this.closing = true;
      try {
        unlinkSync(p);
      } catch {
        /* already gone */
      }
      process.exit(0);
    };
    process.on("SIGINT", cleanup);
    process.on("SIGTERM", cleanup);
  }

  private onSocket(sock: Socket): void {
    let buf = "";
    let live = true;
    sock.on("close", () => {
      live = false;
    });
    sock.on("error", () => {});
    sock.on("data", (d) => {
      buf += d.toString();
      const i = buf.indexOf("\n");
      if (i < 0) return;
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      let req: DaemonRequest;
      try {
        req = JSON.parse(line) as DaemonRequest;
      } catch {
        return void sock.end(JSON.stringify({ ok: false, error: "bad request" }) + "\n");
      }
      void this.handle(req)
        .catch((e): DaemonResponse => ({ ok: false, error: (e as Error).message }))
        .then((res) => live && sock.end(JSON.stringify(res) + "\n"));
    });
  }

  private async handle(req: DaemonRequest): Promise<DaemonResponse> {
    switch (req.op) {
      case "ping":
        return { ok: true, data: { node: this.cfg.nodeName, agents: this.agents.size } };
      case "shutdown": {
        for (const a of [...this.agents.values()]) await this.stopAgent(a.spec.agentId);
        setTimeout(() => process.kill(process.pid, "SIGTERM"), 100);
        return { ok: true };
      }
      case "ls":
        return {
          ok: true,
          data: [...this.agents.values()].map((a) => ({ ...a.spec, cwd: a.cwd, pane: a.paneId })),
        };
      case "up": {
        const adapter = req.adapter ? AdapterId.safeParse(req.adapter) : undefined;
        if (adapter && !adapter.success) return { ok: false, error: `unknown adapter ${req.adapter}` };
        const spec = await this.spawnAgent({ ...req, adapter: adapter?.data });
        return { ok: true, data: spec };
      }
      case "down": {
        let n = 0;
        for (const a of [...this.agents.values()]) {
          if (
            req.agent
              ? a.spec.name === req.agent || a.spec.agentId === req.agent
              : !req.project || a.spec.project === req.project
          ) {
            if (await this.stopAgent(a.spec.agentId)) n++;
          }
        }
        return { ok: true, data: n };
      }
      case "assign":
      case "taskdone": {
        const rt = this.agents.get(req.agentId);
        if (!rt) return { ok: false, error: "unknown agent" };
        rt.noteActivity();
        const frame: NodeFrame =
          req.op === "assign"
            ? { t: "agent.assign", agentId: req.agentId, to: req.to, task: redact(req.task).text, thread: req.thread }
            : { t: "agent.taskdone", agentId: req.agentId, taskId: req.taskId, summary: redact(req.summary).text };
        return this.send(frame)
          ? { ok: true, data: req.op === "assign" ? "assignment sent" : "marked done" }
          : { ok: false, error: "hub offline" };
      }
      case "say":
        if (!this.agents.has(req.agentId)) return { ok: false, error: "unknown agent" };
        this.agents.get(req.agentId)!.noteActivity();
        return this.send({ t: "agent.say", agentId: req.agentId, text: redact(req.text).text, thread: req.thread })
          ? { ok: true }
          : { ok: false, error: "hub offline" };
      case "report":
        if (!this.agents.has(req.agentId)) return { ok: false, error: "unknown agent" };
        return this.send({
          t: "agent.report",
          agentId: req.agentId,
          title: redact(req.title).text,
          summary: redact(req.summary).text,
          artifacts: req.artifacts,
        })
          ? { ok: true }
          : { ok: false, error: "hub offline" };
      case "send": {
        const rt = this.agents.get(req.agentId);
        if (!rt) return { ok: false, error: "unknown agent" };
        const path = resolveInside(rt.cwd, req.path);
        let name = "";
        for await (const c of readChunks(path)) {
          while ((this.ws?.bufferedAmount ?? 0) > 1 << 20) await new Promise((r) => setTimeout(r, 20));
          name = c.name;
          const ok = this.send({
            t: "file.chunk",
            agentId: req.agentId,
            ...c,
            to: req.to,
            caption: req.caption,
            thread: req.thread,
          });
          if (!ok) return { ok: false, error: "hub offline" };
        }
        return { ok: true, data: `sent ${name}${req.to ? ` to ${req.to}` : ""}` };
      }
      case "ask": {
        const rt = this.agents.get(req.agentId);
        if (!rt) return { ok: false, error: "unknown agent" };
        const askId = `${req.agentId}#a${++this.askSeq}`;
        const sent = this.send({
          t: "agent.ask",
          agentId: req.agentId,
          askId,
          question: redact(req.question).text,
          options: req.options,
          thread: req.thread,
        });
        if (!sent) return { ok: false, error: "hub offline" };
        rt.noteActivity();
        rt.beginAsk();
        const answer = await new Promise<string>((resolve) => this.asks.set(askId, { agentId: req.agentId, resolve }));
        rt.endAsk();
        return { ok: true, data: answer };
      }
    }
  }
}
