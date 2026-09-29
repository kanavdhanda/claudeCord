/**
 * Free-tier simulation. Runs the real hub as a separate process and constrains it the way a small VPS would:
 *
 *   CPU:    cgroup-style quota. The process is stopped and resumed inside a 100 ms period, so a 12.5% quota
 *           runs for 12.5 ms and is frozen for 87.5 ms, which is how CFS bandwidth throttling behaves.
 *   Memory: V8 heap cap plus an RSS watchdog that SIGKILLs the process past the limit, like the OOM killer.
 *
 * It is an approximation. Network latency, disk and noisy neighbours are not modelled. CPU fractions are
 * scaled by --derate (default 2) because one core of this machine is faster than a typical cloud vCPU.
 */
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface Tier {
  name: string;
  /** vCPU / OCPU the plan advertises. The hub is single-threaded, so more than 1 does not help it. */
  vcpu: number;
  ramMb: number;
  note: string;
}

export const TIERS: Record<string, Tier> = {
  reference: { name: "reference", vcpu: Infinity, ramMb: 16384, note: "this machine, unconstrained" },
  "oracle-ampere": {
    name: "oracle-ampere",
    vcpu: 2,
    ramMb: 12288,
    note: "Oracle Always Free Ampere A1, 2 OCPU 12 GB (since June 2026)",
  },
  "railway-free": {
    name: "railway-free",
    vcpu: 1,
    ramMb: 512,
    note: "Railway free plan, 1 vCPU 0.5 GB (credit limited)",
  },
  "gcp-e2-micro": {
    name: "gcp-e2-micro",
    vcpu: 0.25,
    ramMb: 1024,
    note: "Google Cloud Always Free e2-micro, 0.25 vCPU sustained 1 GB",
  },
  "oracle-amd-micro": {
    name: "oracle-amd-micro",
    vcpu: 0.125,
    ramMb: 1024,
    note: "Oracle Always Free AMD micro, 1/8 OCPU 1 GB",
  },
  "render-free": {
    name: "render-free",
    vcpu: 0.1,
    ramMb: 512,
    note: "Render free web service, 0.1 CPU 512 MB (sleeps after 15 min idle)",
  },
  "fly-shared-256": {
    name: "fly-shared-256",
    vcpu: 0.0625,
    ramMb: 256,
    note: "Fly.io shared-cpu-1x 256 MB (legacy free allowance), ~1/16 baseline",
  },
};

const PERIOD_MS = 100;
const OS_RESERVE_MB = 64;

export interface TierHub {
  port: number;
  tokens: string[];
  quota: number;
  /** Resolves when the hub process dies, for example OOM killed. */
  dead: Promise<string>;
  cpuSeconds(): number;
  peakRssMb(): number;
  loopP99Ms(): number;
  stop(): Promise<void>;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function ps(pid: number, field: string): string {
  try {
    return execFileSync("ps", ["-o", `${field}=`, "-p", String(pid)], { encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}

function cpuSecondsOf(pid: number): number {
  const t = ps(pid, "cputime");
  if (!t) return 0;
  return t.split(":").reduce((acc, part) => acc * 60 + Number(part), 0);
}

export async function startTierHub(tier: Tier, nodeCount: number, derate: number): Promise<TierHub> {
  const quota = Number.isFinite(tier.vcpu) ? Math.min(tier.vcpu, 1) / derate : 1;
  const limitMb = tier.ramMb - OS_RESERVE_MB;
  const tokensFile = join(tmpdir(), `cc-tokens-${process.pid}-${Date.now()}.json`);
  const serve = new URL("./serve.ts", import.meta.url).pathname;

  const child: ChildProcess = spawn(
    process.execPath,
    [
      ...process.execArgv,
      `--max-old-space-size=${Math.max(64, Math.floor(limitMb * 0.8))}`,
      serve,
      "--nodes",
      String(nodeCount),
      "--port",
      "0",
      "--tokens",
      tokensFile,
    ],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  const pid = child.pid!;
  process.on("exit", () => {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* gone */
    }
  });

  let peakRss = 0;
  let loopMax = 0;
  let alive = true;
  let reason = "";
  const dead = new Promise<string>((resolve) =>
    child.on("exit", (code, sig) => {
      alive = false;
      resolve(reason || `exited code=${code} signal=${sig}`);
    }),
  );

  const port = await new Promise<number>((resolve, reject) => {
    let buf = "";
    child.stdout!.on("data", (d: Buffer) => {
      buf += d.toString();
      for (const line of buf.split("\n")) {
        const m = line.match(/^LISTENING (\d+)/);
        if (m) resolve(Number(m[1]));
        const l = line.match(/loop_p99=([\d.]+)ms/);
        if (l) loopMax = Math.max(loopMax, Number(l[1]));
      }
      buf = buf.slice(buf.lastIndexOf("\n") + 1);
    });
    child.once("exit", () => reject(new Error("hub exited before listening")));
  });

  // CPU quota: run for quota*period, freeze for the rest.
  if (quota < 1) {
    void (async () => {
      while (alive) {
        try {
          process.kill(pid, "SIGCONT");
        } catch {
          return;
        }
        await sleep(PERIOD_MS * quota);
        if (!alive) return;
        try {
          process.kill(pid, "SIGSTOP");
        } catch {
          return;
        }
        await sleep(PERIOD_MS * (1 - quota));
      }
    })();
  }

  // Memory watchdog.
  const watch = setInterval(() => {
    const mb = Number(ps(pid, "rss")) / 1024;
    if (!mb) return;
    peakRss = Math.max(peakRss, mb);
    if (mb > limitMb && alive) {
      reason = `OOM killed at ${Math.round(mb)} MB (limit ${limitMb} MB)`;
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        /* gone */
      }
    }
  }, 200);

  return {
    port,
    tokens: JSON.parse(readFileSync(tokensFile, "utf8")) as string[],
    quota,
    dead,
    cpuSeconds: () => cpuSecondsOf(pid),
    peakRssMb: () => peakRss,
    loopP99Ms: () => loopMax,
    async stop() {
      clearInterval(watch);
      alive = false;
      try {
        process.kill(pid, "SIGCONT");
        process.kill(pid, "SIGKILL");
      } catch {
        /* gone */
      }
      rmSync(tokensFile, { force: true });
    },
  };
}
