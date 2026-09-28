#!/usr/bin/env node
import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync } from "node:fs";
import { hostname } from "node:os";
import { basename, join } from "node:path";
import { createInterface } from "node:readline/promises";
import { callDaemon, currentAgentId, meshDir } from "@claudecord/agent-tools";
import { AdapterId } from "@claudecord/protocol";
import { cliPath, Daemon } from "./daemon.js";
import {
  loadNodeConfig,
  loadProjectConfig,
  saveNodeConfig,
  saveProjectConfig,
  type Policy,
} from "./config.js";
import { attach } from "./tmux.js";

interface Args {
  pos: string[];
  flags: Map<string, string[]>;
}

function parse(argv: string[]): Args {
  const pos: string[] = [];
  const flags = new Map<string, string[]>();
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a.startsWith("--")) {
      const key = a.slice(2);
      const next = argv[i + 1];
      const val = next !== undefined && !next.startsWith("--") ? (i++, next) : "true";
      flags.set(key, [...(flags.get(key) ?? []), val]);
    } else pos.push(a);
  }
  return { pos, flags };
}

const flag = (a: Args, k: string) => a.flags.get(k)?.[0];

const HELP = `claudecord - run coding agents as a Discord team

Setup (once per device)
  claudecord init                     Save hub URL, device token and defaults

Per project (run inside the project folder)
  claudecord up [--project p]         Start the first agent for this folder
  claudecord new [name]               Add another agent to this project
    flags: --adapter claude|agy|codex  --model m  --role r  --policy autonomous|plan|ask
  claudecord down [name]              Stop one agent, or all agents of this project
  claudecord ls                       List agents on this device
  claudecord attach [project]         Attach to the tmux session
  claudecord status                   Check the local daemon
  claudecord daemon                   Run the node daemon in the foreground

Used by agents
  claudecord say "text" [--thread t]
  claudecord ask "question" [--option text]...
  claudecord report --title t "summary" [--artifact text]...
`;

async function init(a: Args): Promise<void> {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  const ask = async (key: string, q: string, def?: string) => {
    const v = flag(a, key);
    if (v) return v;
    const r = (await rl.question(def ? `${q} [${def}]: ` : `${q}: `)).trim();
    return r || def || "";
  };
  const hubUrl = await ask("hub", "hub URL (e.g. wss://hub.example.com)");
  const token = await ask("token", "device token (from /token in Discord)");
  const nodeName = await ask("name", "device name", hostname().split(".")[0]);
  const adapter = await ask("adapter", "default agent (claude/agy/codex)", "claude");
  const model = await ask("model", "default model (blank for CLI default)");
  const role = await ask("role", "default role (blank for none)");
  const policy = await ask("policy", "policy (autonomous/plan/ask)", "ask");
  rl.close();
  const ad = AdapterId.safeParse(adapter);
  if (!ad.success) throw new Error(`unknown agent ${adapter}`);
  if (!["autonomous", "plan", "ask"].includes(policy)) throw new Error(`unknown policy ${policy}`);
  if (!hubUrl || !token) throw new Error("hub URL and token are required");
  saveNodeConfig({
    hubUrl,
    token,
    nodeName,
    adapter: ad.data,
    model: model || undefined,
    role: role || undefined,
    policy: policy as Policy,
  });
  console.log(`Saved. Next: cd into a project and run: claudecord up`);
}

async function daemonUp(): Promise<boolean> {
  try {
    const r = await callDaemon({ op: "ping" }, 1500);
    return r.ok;
  } catch {
    return false;
  }
}

async function ensureDaemon(): Promise<void> {
  if (await daemonUp()) return;
  mkdirSync(meshDir(), { recursive: true });
  const log = openSync(join(meshDir(), "daemon.log"), "a");
  spawn(process.execPath, [cliPath(), "daemon"], { detached: true, stdio: ["ignore", log, log] }).unref();
  closeSync(log);
  for (let i = 0; i < 30; i++) {
    await new Promise((r) => setTimeout(r, 200));
    if (await daemonUp()) return;
  }
  throw new Error(`daemon did not start, see ${join(meshDir(), "daemon.log")}`);
}

async function up(a: Args, forceNew: boolean): Promise<void> {
  loadNodeConfig();
  const cwd = process.cwd();
  let pc = loadProjectConfig(cwd);
  const project = flag(a, "project") ?? pc?.project ?? basename(cwd);
  if (!pc) {
    pc = { project };
    saveProjectConfig(cwd, pc);
  }
  await ensureDaemon();
  if (!forceNew) {
    const ls = await callDaemon({ op: "ls" });
    if (ls.ok && (ls.data as { project: string }[]).some((x) => x.project === project)) {
      console.log(`Project ${project} already has an agent here. Use "claudecord new" to add another.`);
      return;
    }
  }
  const r = await callDaemon({
    op: "up",
    cwd,
    project,
    name: a.pos[0] ?? flag(a, "name"),
    adapter: flag(a, "adapter"),
    model: flag(a, "model"),
    role: flag(a, "role"),
  });
  if (!r.ok) throw new Error(r.error);
  const spec = r.data as { name: string };
  console.log(`Agent ${spec.name} started in #${project}. View it: claudecord attach ${project}`);
  if (flag(a, "attach")) attach(`claude-${project}`);
}

async function main(): Promise<void> {
  const [cmd, ...rest] = process.argv.slice(2);
  const a = parse(rest);
  switch (cmd) {
    case "init":
      return init(a);
    case "up":
      return up(a, false);
    case "new":
      return up(a, true);
    case "daemon":
      return new Daemon().start();
    case "status": {
      const ok = await daemonUp();
      console.log(ok ? "daemon running" : "daemon not running");
      process.exit(ok ? 0 : 1);
      return;
    }
    case "ls": {
      const r = await callDaemon({ op: "ls" });
      if (!r.ok) throw new Error(r.error);
      const rows = r.data as { name: string; project: string; adapter: string; model?: string; pane: string }[];
      if (!rows.length) console.log("No agents running.");
      for (const x of rows) console.log(`${x.project}  ${x.name}  ${x.adapter}${x.model ? `/${x.model}` : ""}  ${x.pane}`);
      return;
    }
    case "down": {
      const project = flag(a, "project") ?? loadProjectConfig(process.cwd())?.project ?? basename(process.cwd());
      const r = await callDaemon({ op: "down", agent: a.pos[0], project });
      if (!r.ok) throw new Error(r.error);
      console.log(`Stopped ${r.data} agent(s).`);
      return;
    }
    case "attach": {
      const project = a.pos[0] ?? loadProjectConfig(process.cwd())?.project ?? basename(process.cwd());
      attach(`claude-${project}`);
      return;
    }
    case "say": {
      const text = a.pos.join(" ");
      if (!text) throw new Error('usage: claudecord say "text" [--thread name]');
      const r = await callDaemon({ op: "say", agentId: currentAgentId(), text, thread: flag(a, "thread") });
      if (!r.ok) throw new Error(r.error);
      return;
    }
    case "ask": {
      const question = a.pos.join(" ");
      if (!question) throw new Error('usage: claudecord ask "question" [--option text]...');
      const r = await callDaemon({
        op: "ask", agentId: currentAgentId(), question, options: a.flags.get("option"), thread: flag(a, "thread"),
      });
      if (!r.ok) throw new Error(r.error);
      console.log(String(r.data));
      return;
    }
    case "report": {
      const summary = a.pos.join(" ");
      const title = flag(a, "title");
      if (!summary || !title) throw new Error('usage: claudecord report --title t "summary" [--artifact text]...');
      const r = await callDaemon({ op: "report", agentId: currentAgentId(), title, summary, artifacts: a.flags.get("artifact") });
      if (!r.ok) throw new Error(r.error);
      return;
    }
    default:
      console.log(HELP);
  }
}

main().catch((e) => {
  console.error(`error: ${(e as Error).message}`);
  process.exit(1);
});
