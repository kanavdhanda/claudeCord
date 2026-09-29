/**
 * Hub load test. Spins up the real hub and gateway in this process with a no-op Discord layer,
 * and drives it from child processes that act as nodes hosting many simulated agents.
 *
 *   tsx bench/load.ts                      ramp: 50 .. 3200 agents
 *   tsx bench/load.ts --random 6 --seed 7  random topologies
 *   tsx bench/load.ts --stages 100,400     custom ramp
 *   tsx bench/load.ts --broadcast 0.2      share of messages without a mention (fan out to peers)
 */
import { fork } from "node:child_process";
import { monitorEventLoopDelay, performance } from "node:perf_hooks";
import type { AddressInfo } from "node:net";
import WebSocket from "ws";
import { NODE_CONNECT_PATH, encode, parseHubFrame, type NodeFrame } from "@claudecord/protocol";
import { Db } from "../src/db.js";
import { startGateway } from "../src/gateway.js";
import { Hub, type Outbound } from "../src/hub.js";
import { TIERS, startTierHub, type Tier, type TierHub } from "./tier.js";

const now = () => performance.timeOrigin + performance.now();

interface Plan {
  url: string;
  nodes: { name: string; token: string }[];
  agents: { node: number; project: string; name: string; peers: string[] }[];
  rate: number;
  broadcast: number;
  durationMs: number;
  seed: number;
}

interface ChildResult {
  sent: number;
  expected: number;
  delivered: number;
  latencies: number[];
  registered: number;
}

// ---------------------------------------------------------------- client (child process)

function rng(seed: number) {
  let s = seed >>> 0;
  return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32);
}

async function runClient(): Promise<void> {
  const plan: Plan = await new Promise((r) => process.once("message", (m) => r(m as Plan)));
  const rand = rng(plan.seed);
  const res: ChildResult = { sent: 0, expected: 0, delivered: 0, latencies: [], registered: 0 };
  const sockets: WebSocket[] = [];
  const byNode = new Map<number, { ws: WebSocket; agents: Plan["agents"] }>();
  plan.agents.forEach((a) => {
    const e = byNode.get(a.node) ?? { ws: undefined as unknown as WebSocket, agents: [] };
    e.agents.push(a);
    byNode.set(a.node, e);
  });

  // Connect with a small stagger and retry, so a thundering herd does not overflow the listen backlog.
  const connect = (idx: number, e: { ws: WebSocket; agents: Plan["agents"] }, attempt = 0) =>
    new Promise<void>((resolve) => {
      const node = plan.nodes[idx]!;
      const ws = new WebSocket(plan.url, { headers: { Authorization: `Bearer ${node.token}` } });
      ws.on("error", () => {
        if (ws.readyState !== ws.OPEN && attempt < 5) setTimeout(() => connect(idx, e, attempt + 1).then(resolve), 100 + Math.random() * 400 * (attempt + 1));
        else if (ws.readyState !== ws.OPEN) resolve();
      });
      ws.on("open", () => {
        e.ws = ws;
        sockets.push(ws);
        ws.send(encode({ t: "hello", nodeName: node.name, version: "bench" }));
        for (const a of e.agents) {
          ws.send(encode({ t: "agent.register", cwd: "/bench", agent: { agentId: `${a.project}/${a.name}`, name: a.name, project: a.project, adapter: "claude" } }));
        }
        resolve();
      });
      ws.on("message", (d) => {
        const f = parseHubFrame(d.toString());
        if (f?.t !== "deliver" || f.from === "system" || f.from === "engineer") return;
        const t = Number(f.text.split("|")[0]);
        if (Number.isFinite(t)) {
          res.delivered++;
          res.latencies.push(now() - t);
        }
      });
    });
  let n = 0;
  await Promise.all([...byNode.entries()].map(([idx, e]) => new Promise<void>((r) => setTimeout(r, 4 * n++)).then(() => connect(idx, e))));
  process.send!({ ready: true });
  await new Promise((r) => process.once("message", r));

  const stopAt = now() + plan.durationMs;
  const timers: Promise<void>[] = plan.agents.map(async (a) => {
    const ws = byNode.get(a.node)!.ws;
    if (!ws) return;
    await new Promise((r) => setTimeout(r, rand() * (1000 / plan.rate)));
    while (now() < stopAt) {
      const targeted = rand() >= plan.broadcast && a.peers.length > 0;
      const peer = a.peers[Math.floor(rand() * a.peers.length)];
      const text = `${now()}|${targeted ? `@${peer} ` : ""}ping`;
      const frame: NodeFrame = { t: "agent.say", agentId: `${a.project}/${a.name}`, text };
      if (ws.readyState === ws.OPEN) {
        ws.send(encode(frame));
        res.sent++;
        res.expected += targeted ? 1 : a.peers.length;
      }
      await new Promise((r) => setTimeout(r, (0.5 + rand()) * (1000 / plan.rate)));
    }
  });
  await Promise.all(timers);
  await new Promise((r) => setTimeout(r, 1500)); // drain
  sockets.forEach((s) => s.close());
  // IPC sends are asynchronous. Exiting before the callback fires drops large results.
  process.send!({ result: res }, () => process.exit(0));
}

// ---------------------------------------------------------------- orchestrator

const noopOut: Outbound = {
  ensureProject: async () => {},
  post: async () => {},
  postAsk: async () => {},
  postReport: async () => {},
  postFile: async () => {},
  confirm: async () => {},
  notice: async () => {},
  refreshStatus: () => {},
};

function pct(sorted: number[], p: number): number {
  return sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))]! : 0;
}

interface StageResult {
  agents: number;
  projects: number;
  nodes: number;
  sentPerSec: number;
  deliveredPct: number;
  p50: number;
  p95: number;
  p99: number;
  loopP99: number;
  cpuPct: number;
  rssMb: number;
  registerSec: number;
  failed?: string;
}

const dbg = (m: string) => process.env.BENCH_DEBUG && console.error(`[${(performance.now() / 1000).toFixed(1)}s] ${m}`);

interface StageOpts {
  rate: number;
  broadcast: number;
  durationMs: number;
  seed: number;
  children: number;
  tier?: Tier;
  derate: number;
}

async function runStage(agentCount: number, sizes: number[], opts: StageOpts): Promise<StageResult> {
  dbg(`stage ${agentCount}: start`);
  const rand = rng(opts.seed);
  const nodeCount = Math.max(1, Math.ceil(agentCount / 25));

  // Either the real hub in this process, or the hub as a separate process under a tier's CPU and memory limits.
  let port: number;
  let nodes: { name: string; token: string }[];
  let closeHub: () => Promise<void>;
  let ext: TierHub | undefined;
  if (opts.tier) {
    ext = await startTierHub(opts.tier, nodeCount, opts.derate);
    port = ext.port;
    nodes = ext.tokens.map((token, i) => ({ name: `n${i}`, token }));
    closeHub = () => ext!.stop();
  } else {
    const db = new Db(":memory:");
    const hub = new Hub(db, Number.MAX_SAFE_INTEGER);
    hub.out = noopOut;
    const server = startGateway(hub, 0);
    await new Promise((r) => server.once("listening", r));
    port = (server.address() as AddressInfo).port;
    nodes = Array.from({ length: nodeCount }, (_, i) => ({ name: `n${i}`, token: db.createToken(`n${i}`) }));
    closeHub = () => new Promise((r) => void server.close(() => r()));
  }

  // Topology: projects of random size, agents spread across nodes of ~25 agents.
  const agents: Plan["agents"] = [];
  let p = 0;
  while (agents.length < agentCount) {
    const size = Math.min(sizes[Math.floor(rand() * sizes.length)]!, agentCount - agents.length);
    const names = Array.from({ length: size }, (_, i) => `a${p}_${i}`);
    names.forEach((name) => agents.push({ node: Math.floor(rand() * nodeCount), project: `p${p}`, name, peers: names.filter((n) => n !== name) }));
    p++;
  }

  dbg("topology built, forking clients");
  const kids = Array.from({ length: opts.children }, () => fork(new URL(import.meta.url), ["--client"], { execArgv: process.execArgv }));
  kids.forEach((k, i) => k.on("exit", (code, sig) => dbg(`client ${i} exited code=${code} signal=${sig}`)));
  const results: ChildResult[] = [];
  const done = kids.map((k) => new Promise<void>((r) => k.on("message", (m: { result?: ChildResult }) => { if (m.result) { results.push(m.result); r(); } })));
  const ready = kids.map((k) => new Promise<void>((r) => k.on("message", (m: { ready?: boolean }) => m.ready && r())));

  // Nodes are partitioned across children so one node's frames stay on one connection.
  const t0 = performance.now();
  kids.forEach((k, i) => {
    const mine = new Set(nodes.map((_, idx) => idx).filter((idx) => idx % opts.children === i));
    const plan: Plan = {
      url: `ws://127.0.0.1:${port}${NODE_CONNECT_PATH}`,
      nodes,
      agents: agents.filter((a) => mine.has(a.node)),
      rate: opts.rate,
      broadcast: opts.broadcast,
      durationMs: opts.durationMs,
      seed: opts.seed + i,
    };
    k.send(plan);
  });
  const failure = ext ? ext.dead.then((why) => ({ why })) : new Promise<never>(() => {});
  const early = await Promise.race([Promise.all(ready).then(() => null), failure]);
  if (early) return abortStage(agentCount, p, nodeCount, early.why, kids, closeHub);
  dbg("all clients registered");
  const registerSec = (performance.now() - t0) / 1000;

  const loop = monitorEventLoopDelay({ resolution: 5 });
  loop.enable();
  const cpu0 = process.cpuUsage();
  const extCpu0 = ext?.cpuSeconds() ?? 0;
  const w0 = performance.now();
  kids.forEach((k) => k.send("go"));
  const limit = setTimeout(() => {
    console.error(`stage ${agentCount}: only ${results.length}/${kids.length} clients reported, aborting`);
    process.exit(2);
  }, opts.durationMs + 60_000);
  const end = await Promise.race([Promise.all(done).then(() => null), failure]);
  clearTimeout(limit);
  if (end) return abortStage(agentCount, p, nodeCount, end.why, kids, closeHub);
  dbg("all clients finished");
  const wall = (performance.now() - w0) / 1000;
  const cpu = process.cpuUsage(cpu0);
  loop.disable();

  const lat = results.flatMap((r) => r.latencies).sort((a, b) => a - b);
  const sent = results.reduce((s, r) => s + r.sent, 0);
  const expected = results.reduce((s, r) => s + r.expected, 0);
  const delivered = results.reduce((s, r) => s + r.delivered, 0);
  const out: StageResult = {
    agents: agentCount,
    projects: p,
    nodes: nodeCount,
    sentPerSec: Math.round(sent / (opts.durationMs / 1000)),
    deliveredPct: expected ? Math.round((delivered / expected) * 1000) / 10 : 0,
    p50: Math.round(pct(lat, 50) * 10) / 10,
    p95: Math.round(pct(lat, 95) * 10) / 10,
    p99: Math.round(pct(lat, 99) * 10) / 10,
    loopP99: Math.round(((ext ? ext.loopP99Ms() * 1e6 : loop.percentile(99)) / 1e6) * 10) / 10,
    // For a tier this is the share of the plan's CPU quota that was used, so ~100 means saturated.
    cpuPct: ext
      ? Math.round(((ext.cpuSeconds() - extCpu0) / (wall * ext.quota)) * 100)
      : Math.round(((cpu.user + cpu.system) / 1e6 / wall) * 100),
    rssMb: Math.round(ext ? ext.peakRssMb() : process.memoryUsage().rss / 1048576),
    registerSec: Math.round(registerSec * 10) / 10,
  };
  kids.forEach((k) => k.kill());
  dbg("closing server");
  await closeHub();
  dbg("server closed");
  return out;
}

async function abortStage(
  agents: number, projects: number, nodes: number, why: string,
  kids: ReturnType<typeof fork>[], closeHub: () => Promise<void>,
): Promise<StageResult> {
  kids.forEach((k) => k.kill());
  await closeHub();
  return { agents, projects, nodes, sentPerSec: 0, deliveredPct: 0, p50: 0, p95: 0, p99: 0, loopP99: 0, cpuPct: 0, rssMb: 0, registerSec: 0, failed: why };
}

function table(rows: StageResult[]): void {
  const cols: [string, keyof StageResult][] = [
    ["agents", "agents"], ["projects", "projects"], ["nodes", "nodes"], ["msg/s", "sentPerSec"],
    ["deliv%", "deliveredPct"], ["p50ms", "p50"], ["p95ms", "p95"], ["p99ms", "p99"],
    ["loop p99", "loopP99"], ["cpu%", "cpuPct"], ["rssMB", "rssMb"], ["reg s", "registerSec"],
  ];
  console.log(cols.map(([h]) => h.padStart(9)).join(""));
  for (const r of rows) {
    if (r.failed) console.log(`${String(r.agents).padStart(9)}  FAILED: ${r.failed}`);
    else console.log(cols.map(([, k]) => String(r[k]).padStart(9)).join(""));
  }
}

async function main(): Promise<void> {
  const arg = (k: string, d: string) => {
    const i = process.argv.indexOf(`--${k}`);
    return i >= 0 ? process.argv[i + 1]! : d;
  };
  const opts = {
    rate: Number(arg("rate", "1")),
    broadcast: Number(arg("broadcast", "0.2")),
    durationMs: Number(arg("seconds", "8")) * 1000,
    seed: Number(arg("seed", "1")),
    children: Number(arg("children", "4")),
    derate: Number(arg("derate", "2")),
    tier: undefined as Tier | undefined,
  };
  const tierName = arg("tier", "");
  if (tierName) {
    opts.tier = TIERS[tierName];
    if (!opts.tier) throw new Error(`unknown tier ${tierName}. Choose from: ${Object.keys(TIERS).join(", ")}`);
  }
  const untilFail = process.argv.includes("--until-fail");
  const isOk = (r: StageResult) => !r.failed && r.deliveredPct >= 99 && r.p99 <= 250;
  const sizes = [2, 3, 4, 6, 8];
  const rows: StageResult[] = [];
  const random = Number(arg("random", "0"));
  let stages: number[];
  if (random) {
    const r = rng(opts.seed);
    stages = Array.from({ length: random }, () => Math.round(20 + r() * 1980));
  } else stages = arg("stages", "50,100,200,400,800,1600,3200").split(",").map(Number);

  if (opts.tier) {
    const q = Number.isFinite(opts.tier.vcpu) ? `${(Math.min(opts.tier.vcpu, 1) / opts.derate * 100).toFixed(1)}% of a core (derate ${opts.derate})` : "unthrottled";
    console.log(`tier ${opts.tier.name}: ${opts.tier.note}\n  simulated CPU ${q}, RAM ${opts.tier.ramMb} MB\n`);
  }
  console.log(`agents send ~${opts.rate} msg/s each, ${opts.broadcast * 100}% broadcast, ${opts.durationMs / 1000}s per stage, ${opts.children} client processes\n`);
  for (const n of stages) {
    const r = await runStage(n, sizes, { ...opts, seed: opts.seed + n });
    rows.push(r);
    console.log(JSON.stringify(r));
    if (untilFail && !isOk(r)) break;
  }
  console.log("\n");
  table(rows);
  const ok = rows.filter(isOk);
  const max = ok.length ? Math.max(...ok.map((r) => r.agents)) : 0;
  console.log(`\nlargest stage meeting delivery >= 99% and p99 <= 250ms: ${max} agents`);
  process.exit(0);
}

if (process.argv.includes("--client")) await runClient();
else await main();
