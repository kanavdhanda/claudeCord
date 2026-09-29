// k6 scenario: each VU is one node daemon hosting many simulated agents.
// Default is 200 nodes x 25 agents = 5000 agents, in projects of 5 agents spread over 5 different nodes.
//
//   terminal 1:  pnpm bench:serve --nodes 200
//   terminal 2:  pnpm bench:k6
//   tune:        k6 run -e NODES=200 -e AGENTS_PER_NODE=25 -e SESSION_S=60 packages/hub/bench/k6/agents.js
import ws from "k6/ws";
import { check, sleep } from "k6";
import { Counter, Rate, Trend } from "k6/metrics";
import { SharedArray } from "k6/data";

const NODES = Number(__ENV.NODES || 200);
const PER_NODE = Number(__ENV.AGENTS_PER_NODE || 25);
const SESSION_S = Number(__ENV.SESSION_S || 60);
const GROUP = 5; // nodes per project group, so every project spans several nodes
const RATE_PER_AGENT = Number(__ENV.RATE || 1); // messages per second per agent
const BROADCAST = Number(__ENV.BROADCAST || 0.2);
const URL = __ENV.HUB_URL || "ws://127.0.0.1:8787/api/v1/node/connect";

const tokens = new SharedArray("tokens", () => JSON.parse(open(__ENV.TOKENS || "./tokens.json")));

const latency = new Trend("deliver_latency_ms", true);
const sent = new Counter("msgs_sent");
const expected = new Counter("msgs_expected");
const delivered = new Counter("msgs_delivered");
const connectFailed = new Rate("ws_connect_failed");
const registered = new Counter("agents_registered");

export const options = {
  // The summary only carries percentiles listed here, and the report below reads p(99).
  summaryTrendStats: ["avg", "med", "p(90)", "p(95)", "p(99)", "max"],
  scenarios: {
    nodes: {
      executor: "per-vu-iterations",
      vus: NODES,
      iterations: 1,
      maxDuration: `${SESSION_S + 150}s`,
    },
  },
  thresholds: {
    deliver_latency_ms: ["p(95)<100", "p(99)<250"],
    ws_connect_failed: ["rate<0.001"],
    msgs_delivered: [`count>0`],
  },
};

function agentName(vu, i) {
  return `v${vu}a${i}`;
}

function project(vu, i) {
  return `p${Math.floor((vu - 1) / GROUP)}_${i}`;
}

function peersOf(vu, i) {
  const g = Math.floor((vu - 1) / GROUP) * GROUP;
  const out = [];
  for (let v = g + 1; v <= Math.min(g + GROUP, NODES); v++) if (v !== vu) out.push(agentName(v, i));
  return out;
}

export default function () {
  const vu = __VU;
  sleep(Math.random() * 5); // spread the connection storm
  const res = ws.connect(URL, { headers: { Authorization: `Bearer ${tokens[vu - 1]}` } }, (socket) => {
    socket.on("open", () => {
      socket.send(JSON.stringify({ t: "hello", nodeName: `n${vu - 1}`, version: "k6" }));
      for (let i = 0; i < PER_NODE; i++) {
        socket.send(
          JSON.stringify({
            t: "agent.register",
            cwd: "/k6",
            agent: {
              agentId: `${project(vu, i)}/${agentName(vu, i)}`,
              name: agentName(vu, i),
              project: project(vu, i),
              adapter: "claude",
            },
          }),
        );
        registered.add(1);
      }
      // Wait for the other nodes in the group to register before chatting.
      socket.setTimeout(
        () => {
          const period = Math.max(1, Math.floor(1000 / (RATE_PER_AGENT * PER_NODE)));
          let n = 0;
          // Everyone stops sending before anyone disconnects, so a peer is never gone when a message to it is sent.
          let sending = true;
          socket.setInterval(() => {
            if (!sending) return;
            const i = n++ % PER_NODE;
            const peers = peersOf(vu, i);
            if (!peers.length) return;
            const targeted = Math.random() >= BROADCAST;
            const peer = peers[Math.floor(Math.random() * peers.length)];
            socket.send(
              JSON.stringify({
                t: "agent.say",
                agentId: `${project(vu, i)}/${agentName(vu, i)}`,
                text: `${Date.now()}|${targeted ? "@" + peer + " " : ""}ping`,
              }),
            );
            sent.add(1);
            expected.add(targeted ? 1 : peers.length);
          }, period);
          socket.setTimeout(() => {
            sending = false;
          }, SESSION_S * 1000);
        },
        8000 + Math.random() * 2000,
      );
    });

    socket.on("message", (data) => {
      const f = JSON.parse(data);
      if (f.t !== "deliver" || f.from === "system" || f.from === "engineer") return;
      const t = Number(String(f.text).split("|")[0]);
      if (t > 0) {
        latency.add(Date.now() - t);
        delivered.add(1);
      }
    });

    socket.on("error", (e) => {
      // A socket that is already closing reports this when the session ends. It is not a failure.
      if (!/close sent|use of closed/i.test(e.error())) console.error(`vu ${vu}: ${e.error()}`);
    });
    // Start jitter (5 s) + send delay (10 s) + send window + time for the last messages to drain.
    socket.setTimeout(() => socket.close(), (SESSION_S + 25) * 1000);
  });
  connectFailed.add(!check(res, { "upgraded to websocket": (r) => r && r.status === 101 }));
}

export function handleSummary(data) {
  const m = (k, f) => (data.metrics[k] && data.metrics[k].values[f]) || 0;
  const expected = m("msgs_expected", "count");
  const delivered = m("msgs_delivered", "count");
  const pct = expected ? (delivered / expected) * 100 : 0;
  const lines = [
    "",
    `agents registered : ${m("agents_registered", "count")}`,
    `messages sent     : ${m("msgs_sent", "count")}  (${m("msgs_sent", "rate").toFixed(0)}/s)`,
    `delivered         : ${delivered} of ${expected} expected (${expected ? pct.toFixed(2) : "n/a"}%)`,
    `latency ms        : p50 ${m("deliver_latency_ms", "med").toFixed(1)}  p95 ${m("deliver_latency_ms", "p(95)").toFixed(1)}  p99 ${m("deliver_latency_ms", "p(99)").toFixed(1)}  max ${m("deliver_latency_ms", "max").toFixed(1)}`,
    "",
  ];
  return {
    stdout: lines.join("\n") + "\n",
    // Read by scripts/ci/run.mjs k6-delivery, because k6 thresholds cannot express a ratio of two counters.
    "k6-summary.json": JSON.stringify({
      sent: m("msgs_sent", "count"),
      expected,
      delivered,
      deliveredPct: pct,
      p99: m("deliver_latency_ms", "p(99)"),
      registered: m("agents_registered", "count"),
    }),
  };
}
