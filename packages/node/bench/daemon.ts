/**
 * What the node daemon costs on a machine while its agents sit idle, per agent. Runs the real packaged daemon as its
 * own process with N idle agents (a stand-in for the Claude Code terminal) and measures steady state only, after
 * startup, in two windows: just after the agents go idle, and later once they have settled.
 *
 *   tsx packages/node/bench/daemon.ts [--agents 0,1,5,10,20] [--active 10] [--settled 20]
 *
 * The daemon's cost is dominated by starting tmux processes, which a CPU sample of the daemon alone cannot see. So the
 * run counts them with a counting shim in front of tmux, measures what one costs, and adds that to the daemon's own
 * CPU and the tmux server's.
 */
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { callDaemon } from "../../agent-tools/src/ipc.js";
import { saveNodeConfig } from "../src/config.js";
import { CLI, IPC, installFakeClaude, makeTmp, sleep, startRecordingHub } from "../test/helpers/e2e.js";

const arg = (k: string, d: string) => {
  const i = process.argv.indexOf(`--${k}`);
  return i >= 0 ? process.argv[i + 1]! : d;
};
const counts = arg("agents", "0,1,5,10,20").split(",").map(Number);
const activeSecs = Number(arg("active", "10"));
const settledSecs = Number(arg("settled", "20"));
const realTmux = spawnSync("which", ["tmux"], { encoding: "utf8" }).stdout.trim();

const cpuSeconds = (pid: number): number => {
  const t = spawnSync("ps", ["-o", "cputime=", "-p", String(pid)], { encoding: "utf8" }).stdout.trim();
  return t ? t.split(":").reduce((a, p) => a * 60 + Number(p), 0) : 0;
};

/** CPU seconds one tmux command costs, measured by running a lot of them. */
function costPerExec(): number {
  const sock = `cc-cal-${process.pid}`;
  spawnSync("tmux", ["-L", sock, "new-session", "-d", "-s", "c", "sleep 60"]);
  const n = 300;
  const t0 = process.resourceUsage();
  const start = Date.now();
  for (let i = 0; i < n; i++) spawnSync("tmux", ["-L", sock, "list-panes", "-a", "-F", "#{pane_id}"]);
  const wall = (Date.now() - start) / 1000;
  const t1 = process.resourceUsage();
  spawnSync("tmux", ["-L", sock, "kill-server"]);
  // resourceUsage covers this process only, so the children's CPU is taken from wall time, which on an idle core is
  // close to their CPU time. It slightly overstates, which makes the estimate conservative.
  void t0;
  void t1;
  return wall / n;
}

interface Window {
  execsPerSec: number;
  cpuPct: number;
}

async function trial(agents: number, perExec: number) {
  const tmp = makeTmp();
  const home = join(tmp, "home");
  const cwd = join(tmp, "proj");
  const sock = `cc-bench-${process.pid}-${agents}`;
  const counter = join(tmp, "execs");
  mkdirSync(cwd, { recursive: true });
  writeFileSync(counter, "");
  const bin = installFakeClaude(tmp);
  // A shim ahead of the real tmux that counts each run, then runs it.
  const shim = join(tmp, "shim");
  mkdirSync(shim);
  writeFileSync(join(shim, "tmux"), `#!/bin/sh\nprintf '%s\\n' "$*" >> "${counter}"\nexec "${realTmux}" "$@"\n`);
  chmodSync(join(shim, "tmux"), 0o755);

  const hub = await startRecordingHub();
  const env = {
    ...process.env,
    CLAUDECORD_HOME: home,
    CLAUDECORD_TMUX_SOCKET: sock,
    FAKE_IPC: IPC,
    PATH: `${shim}:${bin}:${process.env.PATH}`,
  };
  Object.assign(process.env, { CLAUDECORD_HOME: home, CLAUDECORD_TMUX_SOCKET: sock });
  saveNodeConfig({
    hubUrl: `ws://127.0.0.1:${hub.port}`,
    token: hub.token,
    nodeName: "bench",
    adapter: "claude",
    policy: "ask",
  });
  const daemon = spawn(process.execPath, [CLI, "daemon"], { env, stdio: "ignore" });

  for (let i = 0; i < 50; i++) {
    if ((await callDaemon({ op: "ping" }, 500).catch(() => null))?.ok) break;
    await sleep(200);
  }
  for (let i = 0; i < agents; i++) await callDaemon({ op: "up", cwd, project: "bench", name: `a${i}` });
  for (let i = 0; i < 150; i++) {
    const idle = hub.db
      .agentsOfProject("bench")
      .filter((a) => hub.hub.status.get(a.agent_id)?.status === "idle").length;
    if (idle === agents) break;
    await sleep(300);
  }

  const serverPid =
    Number(spawnSync("tmux", ["-L", sock, "display-message", "-p", "#{pid}"], { encoding: "utf8" }).stdout.trim()) || 0;

  const lines = () => readFileSync(counter, "utf8").split("\n").filter(Boolean).length;
  const logWindow = (from: number) => {
    const all = readFileSync(counter, "utf8").split("\n").filter(Boolean).slice(from);
    const kinds = new Map<string, number>();
    for (const l of all) {
      const k = l
        .replace(/%\d+/g, "%N")
        .replace(/--cc-[0-9a-f]+--/g, "SEP")
        .split(" ")
        .slice(0, 7)
        .join(" ");
      kinds.set(k, (kinds.get(k) ?? 0) + 1);
    }
    for (const [k, n] of [...kinds].sort((a, b) => b[1] - a[1]).slice(0, 6))
      console.log(`   ${String(n).padStart(4)}x  ${k}`);
  };

  async function window(seconds: number): Promise<Window> {
    const e0 = lines();
    const d0 = cpuSeconds(daemon.pid!);
    const s0 = serverPid ? cpuSeconds(serverPid) : 0;
    const t0 = Date.now();
    await sleep(seconds * 1000);
    const wall = (Date.now() - t0) / 1000;
    const execs = lines() - e0;
    if (process.env.BENCH_LOG) logWindow(e0);
    const own = cpuSeconds(daemon.pid!) - d0 + (serverPid ? cpuSeconds(serverPid) : 0) - s0;
    return { execsPerSec: execs / wall, cpuPct: ((own + execs * perExec) / wall) * 100 };
  }

  const active = await window(activeSecs);
  await sleep(Math.max(0, 35 - activeSecs) * 1000); // agents settle to the slowest pace after 30 s unchanged
  const settled = await window(settledSecs);
  const rssKb =
    Number(spawnSync("ps", ["-o", "rss=", "-p", String(daemon.pid)], { encoding: "utf8" }).stdout.trim()) || 0;

  await callDaemon({ op: "shutdown" }).catch(() => null);
  await new Promise((r) => daemon.once("close", r));
  spawnSync("tmux", ["-L", sock, "kill-server"]);
  hub.close();
  rmSync(tmp, { recursive: true, force: true });
  return { agents, active, settled, rssMb: Math.round(rssKb / 1024) };
}

const perExec = costPerExec();
console.log(`one tmux command costs about ${(perExec * 1000).toFixed(1)} ms of CPU\n`);
const rows = [];
for (const n of counts) {
  process.stdout.write(`measuring ${n} agent(s)... `);
  rows.push(await trial(n, perExec));
  console.log("done");
}

console.log("\n              just after going idle         settled (idle 35 s+)");
console.log("agents   execs/s   CPU    per agent     execs/s   CPU    per agent    daemon RSS");
for (const r of rows) {
  const per = (w: Window) => (r.agents ? (w.cpuPct / r.agents).toFixed(2) : "-").padStart(7);
  console.log(
    `${String(r.agents).padStart(6)}  ${r.active.execsPerSec.toFixed(1).padStart(7)}  ${r.active.cpuPct.toFixed(1).padStart(5)}%  ${per(r.active)}%   ${r.settled.execsPerSec.toFixed(1).padStart(8)}  ${r.settled.cpuPct.toFixed(1).padStart(5)}%  ${per(r.settled)}%   ${String(r.rssMb).padStart(7)} MB`,
  );
}
process.exit(0);
