// Simulated machines for the capacity and leak runs (scripts/load/capacity.sh): each one is a daemon's WebSocket link to the hub
// with a few agents, with no tmux and no real agent. Node 22+ (built-in WebSocket); needs `ulimit -n` above the machine count.
//
//   node machines.mjs --hub ws://127.0.0.1:PORT --tokens tokens.json --machines 2000 --agents 3 --secs 60 --mode hold
//
// Modes:  hold    connect everyone (at --rate per second), keep the links up, agents change status every --status seconds
//         churn   every machine connects, stays 0.5-2 s, drops, and does it again, for --secs
//         agents  every machine registers and drops one fresh agent every second (with names that are never reused)
//         storm   like hold, and a machine that is cut off reconnects at once (no jitter); prints how long the whole fleet took
//         say     every machine sends numbered agent.say frames (--per-sec each); prints how long the hub took to ack (= on disk)
//         files   every machine sends --file-kb files in 48 KB chunks every 2 s; every other one is abandoned half way
// Prints one JSON line at the end: connects, failures by reason, welcome latency p50/p95/p99, reconnect time, frames refused.
import fs from 'node:fs';

const arg = (k, d) => { const i = process.argv.indexOf('--' + k); return i > 0 ? process.argv[i + 1] : d; };
const HUB = arg('hub'), N = +arg('machines', 100), A = +arg('agents', 3), SECS = +arg('secs', 30), RATE = +arg('rate', 200);
const MODE = arg('mode', 'hold'), STATUS = +arg('status', 5), FILE_KB = +arg('file-kb', 256), OFFSET = +arg('offset', 0);
const tokens = JSON.parse(fs.readFileSync(arg('tokens')));
const all = new Set(), welcome = [], acks = [], fails = {}, stats = { connects: 0, refused: 0, closes: 0, delivers: 0, up: 0 };
const bump = (k) => (fails[k] = (fails[k] || 0) + 1);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (a, p) => (a.length ? [...a].sort((x, y) => x - y)[Math.min(a.length - 1, Math.floor(a.length * p))] : null);
const stopAt = Date.now() + SECS * 1000;
let stormCut = 0, stormBack = [];

function machine(i, onWelcome) {
  const me = tokens[(OFFSET + i) % tokens.length];
  const t0 = Date.now();
  const ws = new WebSocket(`${HUB}/api/v1/node/connect`, { headers: { Authorization: `Bearer ${me.token}` } });
  const m = { ws, me, i, timers: [], agents: [] };
  const send = (f) => { try { ws.send(JSON.stringify(f)); } catch { /* closed */ } };
  m.send = send;
  let opened = false;
  ws.onopen = () => { opened = true; send({ t: 'hello', nodeName: me.node, version: 'load' }); };
  ws.onmessage = (e) => {
    let f; try { f = JSON.parse(e.data); } catch { return; }
    if (f.t === 'welcome') {
      stats.connects++; stats.up++; welcome.push(Date.now() - t0);
      for (let a = 0; a < A; a++) {
        const id = `${me.node}/a${a}`;
        send({ t: 'agent.register', agent: { agentId: id, name: `a${a}`, project: me.node, adapter: 'claude' }, cwd: '/w' });
        m.agents.push(id);
      }
      send({ t: 'agents.here', agentIds: m.agents });
      onWelcome?.(m);
    } else if (f.t === 'error') { stats.refused++; bump('hub error: ' + String(f.message).slice(0, 40)); }
    else if (f.t === 'deliver') stats.delivers++;
    else if (f.t === 'ack') {
      // an ack covers every numbered frame up to n: the change is on disk
      m.maxAck = Math.max(m.maxAck ?? 0, f.n); all.add(m);
      for (const [k, at] of m.sent ?? []) if (k <= f.n) { acks.push(Date.now() - at); m.sent.delete(k); }
    }
  };
  ws.onerror = () => { if (!opened) bump('connect failed'); };
  ws.onclose = (e) => { stats.closes++; if (opened) stats.up--; m.timers.forEach(clearInterval); m.closed = true; m.onclose?.(e); };
  return m;
}

async function ramp(each) {
  for (let i = 0; i < N; i++) {
    each(i);
    if ((i + 1) % Math.max(1, Math.floor(RATE / 20)) === 0) await sleep(50);
  }
}

async function main() {
  if (MODE === 'hold' || MODE === 'storm') {
    const up = (m) => {
      m.timers.push(setInterval(() => m.agents.length && m.send({ t: 'agent.status', agentId: m.agents[(Date.now() / 1000 | 0) % m.agents.length], status: Date.now() % 2 ? 'thinking' : 'idle' }), STATUS * 1000));
      if (stormCut) stormBack.push(Date.now() - stormCut);
    };
    const start = (i) => {
      const m = machine(i, up);
      m.onclose = () => { if (MODE === 'storm' && Date.now() < stopAt) { if (!stormCut) stormCut = Date.now(); setTimeout(() => start(i), 0); } };
    };
    await ramp(start);
    stats.rampMs = Date.now() - (stopAt - SECS * 1000);
    while (Date.now() < stopAt) await sleep(500);
  } else if (MODE === 'churn') {
    await Promise.all(Array.from({ length: N }, async (_, i) => {
      await sleep(Math.random() * 2000);
      while (Date.now() < stopAt) {
        const m = machine(i, null);
        await sleep(500 + Math.random() * 1500);
        try { m.ws.close(); } catch { /* */ }
        await sleep(50);
      }
    }));
  } else if (MODE === 'agents') {
    await ramp((i) => {
      let n = 0, prev = null;
      machine(i, (m) => m.timers.push(setInterval(() => {
        // register a new agent and drop the last one: ids are never reused.
        const name = `x${OFFSET}n${n++}`, id = `${m.me.node}/${name}`;
        m.send({ t: 'agent.register', agent: { agentId: id, name, project: m.me.node, adapter: 'claude' }, cwd: '/w' });
        if (prev) m.send({ t: 'agent.gone', agentId: prev });
        // each registered id also speaks once (leaves a streak entry behind), as a real agent does
        m.send({ t: 'agent.say', agentId: id, text: 'hello ' + n });
        prev = id;
      }, 1000)));
    });
    while (Date.now() < stopAt) await sleep(500);
  } else if (MODE === 'say') {
    // every machine sends numbered agent.say frames at --per-sec; the hub acks each once it is on disk, which is what is timed
    const PER = +arg('per-sec', 5), epoch = Date.now();
    await ramp((i) => machine(i, (m) => {
      m.sent = new Map(); let n = 0;
      m.timers.push(setInterval(() => { n++; m.sent.set(n, Date.now()); m.send({ e: epoch, n, t: 'agent.say', agentId: m.agents[0], text: 'status ' + n }); stats.said = (stats.said || 0) + 1; }, 1000 / PER));
    }));
    stats.rampMs = Date.now() - (stopAt - SECS * 1000);
    while (Date.now() < stopAt) await sleep(500);
  } else if (MODE === 'files') {
    await ramp((i) => machine(i, (m) => m.timers.push(setInterval(() => {
      const id = `t${i}-${Date.now()}`, chunk = Buffer.alloc(48 * 1024, 1).toString('base64');
      const chunks = Math.max(2, Math.ceil(FILE_KB / 48)), abandon = (Date.now() / 1000 | 0) % 2 === 0;
      // an abandoned transfer stops before its last piece and is never mentioned again
      for (let c = 0; c < (abandon ? chunks >> 1 : chunks); c++) m.send({ t: 'file.chunk', agentId: m.agents[0], transferId: id, name: 'f.bin', seq: c, last: c === chunks - 1, data: chunk });
    }, 2000))));
    while (Date.now() < stopAt) await sleep(500);
  }
  await sleep(200);
  // what the hub said it had on disk, so a run that kills the hub can check nothing acknowledged is missing
  if (arg('acked')) fs.writeFileSync(arg('acked'), String([...all].reduce((t, m) => t + m.maxAck, 0)));
  console.log(JSON.stringify({
    mode: MODE, machines: N, agents: A, ...stats, failures: fails,
    welcome_ms: { p50: pct(welcome, 0.5), p95: pct(welcome, 0.95), p99: pct(welcome, 0.99), max: pct(welcome, 1) },
    ack_ms: acks.length ? { n: acks.length, per_sec: Math.round(acks.length / SECS), p50: pct(acks, 0.5), p95: pct(acks, 0.95), p99: pct(acks, 0.99), max: pct(acks, 1) } : undefined,
    storm_back_ms: stormBack.length ? { n: stormBack.length, p50: pct(stormBack, 0.5), p99: pct(stormBack, 0.99), max: pct(stormBack, 1) } : undefined,
  }));
  process.exit(0);
}
main();
