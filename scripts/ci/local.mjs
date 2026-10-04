#!/usr/bin/env node
/**
 * Runs what CI runs, in the same order, on this machine.  pnpm ci:local
 *
 *   pnpm ci:local              everything
 *   pnpm ci:local --fast       skip the end to end tests, coverage, performance and k6
 *
 * Steps that need something this machine does not have (tmux, k6) are skipped and say so.
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync, rmSync } from "node:fs";

const fast = process.argv.includes("--fast");
const win = process.platform === "win32";
const pnpm = win ? "pnpm.cmd" : "pnpm";
const have = (cmd, args = ["-V"]) => spawnSync(cmd, args, { stdio: "ignore", shell: win }).status === 0;
const results = [];

function step(name, cmd, args, opts = {}) {
  const t = Date.now();
  process.stdout.write(`\n== ${name}\n`);
  const r = spawnSync(cmd, args, { stdio: "inherit", shell: win, ...opts });
  const secs = ((Date.now() - t) / 1000).toFixed(1);
  results.push({ name, ok: r.status === 0, secs });
  if (r.status !== 0) finish();
}

function skip(name, why) {
  console.log(`\n== ${name}\n   skipped: ${why}`);
  results.push({ name, ok: true, skipped: why, secs: "0.0" });
}

function finish() {
  console.log("\n" + "-".repeat(60));
  for (const r of results)
    console.log(
      `${r.skipped ? "skip" : r.ok ? "ok  " : "FAIL"}  ${r.secs.padStart(6)}s  ${r.name}${r.skipped ? `  (${r.skipped})` : ""}`,
    );
  const bad = results.some((r) => !r.ok);
  console.log(bad ? "\nCI would fail." : "\nCI would pass.");
  process.exit(bad ? 1 : 0);
}

step("format", pnpm, ["format:check"]);
step("lint", pnpm, ["lint"]);
step("typecheck", pnpm, ["typecheck"]);
step("build", pnpm, ["build"]);
step("unit tests", pnpm, ["test"]);
step("packaged CLI smoke test", process.execPath, ["scripts/ci/smoke-package.mjs"]);
step("audit (production)", pnpm, ["audit", "--prod"]);

if (fast) {
  skip("end to end tests, coverage, performance, k6", "--fast");
  finish();
}

if (win || !have("tmux"))
  skip("end to end tests and coverage", win ? "tmux does not exist on native Windows" : "tmux is not installed");
else step("end to end tests and coverage (real tmux)", pnpm, ["test:coverage"]);

step("performance budget", pnpm, [
  "bench",
  "--",
  "--stages",
  "500,2000",
  "--seconds",
  "6",
  "--assert",
  "--max-p99",
  "100",
  "--min-delivered",
  "99.5",
  "--max-rss",
  "400",
]);

if (!have("k6", ["version"])) skip("k6 smoke test", "k6 is not installed");
else {
  const hub = spawn(pnpm, ["bench:serve", "--nodes", "16", "--port", "8787"], {
    stdio: "ignore",
    shell: win,
    detached: !win,
  });
  const up = async () => {
    for (let i = 0; i < 40; i++) {
      try {
        if ((await fetch("http://localhost:8787/healthz")).ok) return true;
      } catch {
        /* not up yet */
      }
      await new Promise((r) => setTimeout(r, 500));
    }
    return false;
  };
  if (!(await up())) {
    results.push({ name: "k6 smoke test", ok: false, secs: "0.0" });
    spawnSync("pkill", ["-f", "[b]ench/serve.ts"]);
    finish();
  }
  try {
    step("k6 smoke test (400 agents)", "k6", [
      "run",
      "--quiet",
      "--no-usage-report",
      "-e",
      "NODES=16",
      "-e",
      "SESSION_S=15",
      "-e",
      "TOKENS=./tokens.json",
      "packages/hub/bench/k6/agents.js",
    ]);
    step("k6 delivery check", process.execPath, ["scripts/ci/run.mjs", "k6-delivery", "k6-summary.json"], {
      env: { ...process.env, K6_AGENTS: "400" },
    });
  } finally {
    if (!win) spawnSync("pkill", ["-f", "[b]ench/serve.ts"]);
    else hub.kill();
    if (existsSync("k6-summary.json")) rmSync("k6-summary.json");
  }
}

finish();
