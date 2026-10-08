// k6 load test for the hub: many simulated users, each a machine with three bots (a lead and two workers) that talk
// to each other through the hub at the same time.
//
// Prepare (on the hub's host):
//   claudecord-hub load-tokens --count 10000 --out tokens.json --data claudecord-hub
//   claudecord-hub hub --data claudecord-hub --bind 0.0.0.0:8787 --allow-plain     # or behind TLS
// Run (raise the file limit first: ulimit -n 65535):
//   k6 run -e HUB=ws://HOST:8787 -e USERS=10000 -e TOKENS=tokens.json -e HOLD=120 scripts/load/hub.js
//
// What each user does: connect, say hello, register three agents, then for HOLD seconds the workers report to the lead
// and the lead answers them, one agent asks a question now and then, and statuses change. The test fails (thresholds
// below) if connects fail, the hub is slow to answer, or messages are not delivered. USERS ramps up over RAMP seconds so
// the hub is not hit by a storm unless you set RAMP low on purpose.
import ws from 'k6/ws';
import { check, sleep } from 'k6';
import { Counter, Trend } from 'k6/metrics';
import { SharedArray } from 'k6/data';

const USERS = Number(__ENV.USERS || 100);
const RAMP = Number(__ENV.RAMP || 60);
const HOLD = Number(__ENV.HOLD || 60);
const HUB = __ENV.HUB || 'ws://127.0.0.1:8787';
const tokens = new SharedArray('tokens', () => JSON.parse(open(__ENV.TOKENS || 'tokens.json')));

const welcomeMs = new Trend('welcome_ms', true);
const deliverMs = new Trend('deliver_ms', true);
const delivered = new Counter('messages_delivered');
const sent = new Counter('messages_sent');
const refused = new Counter('frames_refused');
const agentsUp = new Counter('agents_registered');

export const options = {
  scenarios: {
    users: { executor: 'ramping-vus', startVUs: 0, stages: [{ duration: `${RAMP}s`, target: USERS }, { duration: `${HOLD}s`, target: USERS }, { duration: '10s', target: 0 }], gracefulRampDown: '30s' },
  },
  thresholds: {
    ws_connecting: ['p(95)<2000'],
    welcome_ms: ['p(95)<3000'],
    deliver_ms: ['p(95)<3000'],
    checks: ['rate>0.99'],
    frames_refused: ['count==0'],
  },
};

export default function () {
  // Each virtual user is a distinct machine with its own token and its own project, so users never share state.
  const me = tokens[(__VU - 1) % tokens.length];
  const project = me.node;
  const ids = ['lead', 'ann', 'bob'].map((n) => ({ name: n, id: `${project}/${n}` }));
  const stamps = new Map();
  const res = ws.connect(`${HUB}/api/v1/node/connect`, { headers: { Authorization: `Bearer ${me.token}` } }, (socket) => {
    const t0 = Date.now();
    const send = (f) => socket.send(JSON.stringify(f));
    socket.on('open', () => send({ t: 'hello', nodeName: me.node, version: 'k6' }));
    socket.on('message', (raw) => {
      let f;
      try { f = JSON.parse(raw); } catch (e) { return; }
      if (f.t === 'welcome') {
        welcomeMs.add(Date.now() - t0);
        for (const a of ids) {
          send({ t: 'agent.register', agent: { agentId: a.id, name: a.name, project, adapter: 'claude' }, cwd: `/work/${a.name}` });
          agentsUp.add(1);
        }
        let n = 0;
        socket.setInterval(() => {
          n++;
          const worker = ids[1 + (n % 2)];
          const key = `${project}-${n}`;
          stamps.set(key, Date.now());
          send({ t: 'agent.status', agentId: worker.id, status: n % 2 ? 'thinking' : 'idle' });
          send({ t: 'agent.say', agentId: worker.id, text: `@lead progress ${key}` });
          send({ t: 'agent.say', agentId: ids[0].id, text: `@${worker.name} ack ${key}` });
          sent.add(2);
          if (n % 10 === 0) send({ t: 'agent.ask', agentId: worker.id, askId: `a${n}`, question: `which option for ${key}?` });
        }, 2000);
        socket.setTimeout(() => socket.close(), HOLD * 1000 + RAMP * 1000);
      } else if (f.t === 'deliver') {
        delivered.add(1);
        const m = /([a-z0-9-]+-\d+)/.exec(f.text || '');
        if (m && stamps.has(m[1])) deliverMs.add(Date.now() - stamps.get(m[1]));
      } else if (f.t === 'error') {
        refused.add(1);
      }
    });
    socket.on('error', () => refused.add(1));
  });
  check(res, { 'connected (101)': (r) => r && r.status === 101 });
  sleep(1);
}
