/**
 * Standalone hub for load testing. Real gateway, router and SQLite layer, with a no-op Discord layer.
 * Writes device tokens to a JSON file that the k6 scenario reads.
 *
 *   tsx bench/serve.ts [--nodes 200] [--port 8787] [--tokens bench/k6/tokens.json]
 */
import { writeFileSync } from "node:fs";
import { monitorEventLoopDelay } from "node:perf_hooks";
import { Db } from "../src/db.js";
import { startGateway } from "../src/gateway.js";
import { Hub, type Outbound } from "../src/hub.js";

const arg = (k: string, d: string) => {
  const i = process.argv.indexOf(`--${k}`);
  return i >= 0 ? process.argv[i + 1]! : d;
};
const nodes = Number(arg("nodes", "200"));
const port = Number(arg("port", "8787"));
const tokensFile = arg("tokens", new URL("./k6/tokens.json", import.meta.url).pathname);

const db = new Db(":memory:");
const tokens = Array.from({ length: nodes }, (_, i) => db.createToken(`n${i}`));
writeFileSync(tokensFile, JSON.stringify(tokens));

const hub = new Hub(db, Number.MAX_SAFE_INTEGER);
hub.out = {
  ensureProject: async () => {},
  post: async () => {},
  postAsk: async () => {},
  postReport: async () => {},
  postFile: async () => {},
  confirm: async () => {},
  notice: async () => {},
  refreshStatus: () => {},
} satisfies Outbound;
const server = startGateway(hub, port);
server.once("listening", () => console.log(`LISTENING ${(server.address() as import("node:net").AddressInfo).port}`));

const loop = monitorEventLoopDelay({ resolution: 5 });
loop.enable();
let last = process.cpuUsage();
let lastT = Date.now();
let peakRss = 0;
let peakAgents = 0;
let peakLoopP99 = 0;

setInterval(() => {
  const cpu = process.cpuUsage(last);
  const dt = (Date.now() - lastT) / 1000;
  last = process.cpuUsage();
  lastT = Date.now();
  const agents = db.allAgents().length;
  const rss = process.memoryUsage().rss / 1048576;
  const lp99 = loop.percentile(99) / 1e6;
  loop.reset();
  peakRss = Math.max(peakRss, rss);
  peakAgents = Math.max(peakAgents, agents);
  peakLoopP99 = Math.max(peakLoopP99, lp99);
  console.log(
    `agents=${agents} nodes=${hub.nodes.size} cpu=${Math.round(((cpu.user + cpu.system) / 1e6 / dt) * 100)}% rss=${Math.round(rss)}MB loop_p99=${lp99.toFixed(1)}ms`,
  );
}, 5000);

process.on("SIGINT", () => {
  console.log(`\npeak: agents=${peakAgents} rss=${Math.round(peakRss)}MB loop_p99=${peakLoopP99.toFixed(1)}ms`);
  process.exit(0);
});
console.log(`bench hub on :${port}, ${nodes} node tokens in ${tokensFile}`);
