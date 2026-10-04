#!/usr/bin/env node
/**
 * Installs the packed npm package into an empty folder and runs it, the way `npx claudecord` would on a new machine.
 * Runs on every operating system in CI. Uses no shell syntax, so it behaves the same on Windows.
 *
 *   node scripts/ci/smoke-package.mjs [path/to/package]
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const pkgDir = resolve(process.argv[2] ?? "packages/node");
const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const work = mkdtempSync(join(tmpdir(), "cc-smoke-"));
let failures = 0;

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, {
    encoding: "utf8",
    shell: process.platform === "win32" && cmd.endsWith(".cmd"),
    ...opts,
  });
  return { code: r.status, out: `${r.stdout ?? ""}${r.stderr ?? ""}` };
}

function check(name, ok, detail = "") {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${!ok && detail ? `\n${detail}` : ""}`);
  if (!ok) failures++;
}

try {
  const pack = run(npm, ["pack", "--pack-destination", work, "--json"], { cwd: pkgDir });
  check("npm pack succeeds", pack.code === 0, pack.out);
  const tarball = join(work, JSON.parse(pack.out.slice(pack.out.indexOf("["))).at(-1).filename);
  check("tarball exists", existsSync(tarball));

  const app = join(work, "app");
  spawnSync(process.execPath, ["-e", `require("fs").mkdirSync(${JSON.stringify(app)},{recursive:true})`]);
  run(npm, ["init", "-y"], { cwd: app });
  const install = run(npm, ["install", tarball, "--no-audit", "--no-fund"], { cwd: app });
  check("installs into an empty folder with no workspace", install.code === 0, install.out);

  const cli = join(app, "node_modules", "claudecord", "dist", "cli.js");
  const mcp = join(app, "node_modules", "claudecord", "dist", "mcp.js");
  check("ships the cli and the mcp server, nothing else needed", existsSync(cli) && existsSync(mcp));
  const deps =
    JSON.parse(readFileSync(join(app, "node_modules", "claudecord", "package.json"), "utf8")).dependencies ?? {};
  check("has no runtime dependencies to download", Object.keys(deps).length === 0, JSON.stringify(deps));

  const home = join(work, "home");
  const env = { ...process.env, CLAUDECORD_HOME: home, CLAUDECORD_TMUX_SOCKET: "cc-smoke" };
  const node = (args, input = "") => run(process.execPath, [cli, ...args], { cwd: app, env, input });

  const help = node([]);
  check("prints help", help.code === 0 && help.out.includes("claudecord login"), help.out);

  const init = node(["init", "--hub", "wss://hub.example.com", "--token", "ccn1.smoke", "--name", "smoke"]);
  check("init saves a config without prompting", init.code === 0 && existsSync(join(home, "config.json")), init.out);
  if (process.platform !== "win32") {
    check("the config is private to the user", (statSync(join(home, "config.json")).mode & 0o077) === 0);
  }

  const status = node(["status"]);
  check(
    "status reports the daemon is not running",
    status.code === 1 && status.out.includes("not running"),
    status.out,
  );

  const say = node(["say", "hello"]);
  check(
    "agent commands refuse to run outside an agent",
    say.code !== 0 && say.out.includes("CLAUDECORD_AGENT_ID"),
    say.out,
  );

  const login = node(["login", "http://127.0.0.1:1", "AAAA-AAAA"]);
  check("login explains an unreachable hub", login.code !== 0 && /could not reach/.test(login.out), login.out);

  const handshake = run(process.execPath, [mcp], {
    cwd: app,
    env: { ...env, CLAUDECORD_AGENT_ID: "p/a" },
    input:
      JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "smoke", version: "0" } },
      }) + "\n",
    timeout: 20_000,
  });
  check("the mcp server answers an initialize request", handshake.out.includes('"serverInfo"'), handshake.out);
} finally {
  rmSync(work, { recursive: true, force: true });
}

console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exit(failures ? 1 : 0);
