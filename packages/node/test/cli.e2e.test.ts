/**
 * The packaged CLI bundle, run as separate processes the way a user runs it: login, up, new, ls, down, stop.
 * Needs tmux and a built workspace (pnpm build). Skipped otherwise.
 *
 * The hub runs in this process, so the CLI must be started without blocking the event loop. A synchronous spawn
 * would freeze the hub while the CLI waits for it to answer.
 */
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { CLI, IPC, e2eReady, installFakeClaude, makeTmp, paneDump, startRecordingHub, waitFor as wait } from "./helpers/e2e.js";

vi.setConfig({ testTimeout: 90_000, hookTimeout: 90_000 });

interface Result {
  status: number | null;
  stdout: string;
  stderr: string;
}

describe.skipIf(!e2eReady)("packaged CLI", () => {
  const tmp = makeTmp();
  const home = join(tmp, "home");
  // A folder name with a space, to prove it becomes a valid project name.
  const dir = join(tmp, `cli proj ${process.pid}`);
  const project = `cli-proj-${process.pid}`;
  const sock = `cc-cli-${process.pid}`;
  let h: Awaited<ReturnType<typeof startRecordingHub>>;
  let env: NodeJS.ProcessEnv;

  /** Runs the CLI with its own environment, never blocking this process. */
  const exec = (args: string[], opts: { env?: NodeJS.ProcessEnv; cwd?: string; input?: string } = {}) =>
    new Promise<Result>((resolve) => {
      const p = spawn(process.execPath, [CLI, ...args], { cwd: opts.cwd ?? dir, env: opts.env ?? env, stdio: ["pipe", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      p.stdout.on("data", (d: Buffer) => (stdout += d));
      p.stderr.on("data", (d: Buffer) => (stderr += d));
      p.on("close", (status) => resolve({ status, stdout, stderr }));
      p.stdin.end(opts.input ?? "");
    });
  const run = (...args: string[]) => exec(args);
  const ok = async (...args: string[]) => {
    const r = await run(...args);
    if (r.status !== 0) throw new Error(`claudecord ${args.join(" ")} failed: ${r.stderr}${r.stdout}`);
    return r.stdout;
  };
  const withHome = (name: string): NodeJS.ProcessEnv => ({ ...env, CLAUDECORD_HOME: join(tmp, name) });
  const waitFor = <T,>(what: string, fn: () => T | undefined | false) => wait(what, fn, 25_000, () => paneDump(sock));
  const agents = () => h.hub.db.agentsOfProject(project);
  const hubUrl = () => `http://127.0.0.1:${h.port}`;

  beforeAll(async () => {
    mkdirSync(dir, { recursive: true });
    const bin = installFakeClaude(tmp);
    env = { ...process.env, CLAUDECORD_HOME: home, CLAUDECORD_TMUX_SOCKET: sock, FAKE_IPC: IPC, PATH: `${bin}:${process.env.PATH}` };
    h = await startRecordingHub();
  });

  afterAll(async () => {
    await run("stop");
    spawnSync("tmux", ["-L", sock, "kill-server"]);
    h?.close();
    rmSync(tmp, { recursive: true, force: true });
  });

  it("login pairs the device with a one-time code and saves a private config, with no token pasted", async () => {
    const { code } = h.auth.createPairCode();
    const out = await ok("login", hubUrl(), code, "--name", "cli-node");
    expect(out).toContain('Connected as "cli-node"');
    const cfg = JSON.parse(readFileSync(join(home, "config.json"), "utf8")) as { token: string; hubUrl: string; nodeName: string; policy: string; envPolicy: string };
    expect(cfg.hubUrl).toBe(`ws://127.0.0.1:${h.port}`);
    expect(cfg.nodeName).toBe("cli-node");
    expect(cfg.policy).toBe("ask");
    expect(cfg.envPolicy).toBe("scrub");
    expect(h.db.nodeForToken(cfg.token)).toBe("cli-node");
    if (process.platform !== "win32") {
      expect(statSync(join(home, "config.json")).mode & 0o077).toBe(0);
      expect(statSync(home).mode & 0o077).toBe(0);
    }
    // The code is spent.
    const again = await run("login", hubUrl(), code, "--name", "other");
    expect(again.status).not.toBe(0);
    expect(again.stderr).toMatch(/wrong, used or expired/);
  });

  it("login explains a wrong code and an unreachable hub, and takes a code typed any way", async () => {
    const other = withHome("home-x");
    const wrong = await exec(["login", hubUrl(), "AAAA-AAAA"], { env: other });
    expect(wrong.status).not.toBe(0);
    expect(wrong.stderr).toContain("/connect in Discord");
    const down = await exec(["login", "http://127.0.0.1:1", "AAAA-AAAA"], { env: other });
    expect(down.stderr).toMatch(/could not reach/);
    const { code } = h.auth.createPairCode();
    const lower = await exec(["login", hubUrl(), code.toLowerCase().replace("-", " "), "--name", "typed"], { env: other });
    expect(lower.status).toBe(0);
  });

  it("login accepts the wss address the hub reports, as well as http and https", async () => {
    const { code } = h.auth.createPairCode();
    const r = await exec(["login", `ws://127.0.0.1:${h.port}`, code, "--name", "viaws"], { env: withHome("home-ws") });
    expect(r.status).toBe(0);
  });

  it("without a terminal, login and up say what to do instead of hanging", async () => {
    const other = withHome("home-fresh");
    const noArgs = await exec(["login"], { env: other });
    expect(noArgs.status).not.toBe(0);
    expect(noArgs.stderr).toContain("/connect in Discord");
    const up = await exec(["up"], { env: other });
    expect(up.status).not.toBe(0);
    expect(up.stderr).toContain("npx claudecord login");
    // Plain `claudecord` on an unconfigured machine with no terminal just shows help.
    expect((await exec([], { env: other })).stdout).toContain("First run");
  });

  it("init still works for a token you already have, without prompting", async () => {
    const r = await exec(["init", "--hub", `ws://127.0.0.1:${h.port}`, "--token", h.token, "--name", "manual"], { env: withHome("home-init") });
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("Saved");
  });

  it("warns about an unencrypted hub and about the autonomous policy", async () => {
    const r = await exec(["init", "--hub", "ws://hub.example.com", "--token", "t", "--policy", "autonomous"], { env: withHome("home2") });
    expect(r.stderr).toContain("not encrypted");
    expect(r.stderr).toContain("autonomous skips every permission prompt");
  });

  it("warns when login is pointed at an unencrypted public hub", async () => {
    const r = await exec(["login", "http://hub.invalid", "AAAA-AAAA"], { env: withHome("home-warn") });
    expect(r.stderr).toContain("not encrypted");
  });

  it("up starts the first agent under a valid project name and registers it as lead", async () => {
    expect(await ok("up")).toContain(`#${project}`);
    await waitFor("agent registered", () => agents().length === 1);
    expect(agents()[0]!.is_lead).toBe(1);
    expect((await run("status")).stdout).toContain("daemon running");
  });

  it("a second up in the same project does not start another agent", async () => {
    expect(await ok("up")).toContain("already has an agent");
    expect(agents()).toHaveLength(1);
  });

  it("delivers a message and the agent accepts it", async () => {
    const before = h.confirms.length;
    h.hub.humanMessage(project, "say: from the cli test", undefined, "c:1");
    await waitFor("acceptance", () => h.confirms.length > before);
    await waitFor("agent reply", () => h.posts.some((p) => p.includes("from the cli test")));
  });

  it("new adds a named peer and ls lists both", async () => {
    await ok("new", "peer1");
    await waitFor("peer registered", () => agents().length === 2);
    const out = await ok("ls");
    expect(out).toContain("peer1");
    expect(out.split("\n").filter((l) => l.includes(project))).toHaveLength(2);
  });

  it("rejects an invalid agent name instead of starting it", async () => {
    const r = await run("new", "bad name");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("invalid agent");
  });

  it("agent commands refuse to run outside an agent", async () => {
    const r = await run("say", "hello");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("CLAUDECORD_AGENT_ID");
  });

  it("down stops one agent, stop stops the rest and the daemon", async () => {
    expect(await ok("down", "peer1")).toContain("Stopped 1");
    await waitFor("peer gone", () => agents().length === 1);
    await ok("stop");
    await waitFor("daemon stopped", () => spawnSync(process.execPath, [CLI, "status"], { cwd: dir, env }).status !== 0);
    await waitFor("tmux session gone", () => spawnSync("tmux", ["-L", sock, "has-session", "-t", `=claude-${project}`]).status !== 0);
  });

  it("with no tmux the CLI explains how to fix it instead of failing obscurely", async () => {
    const r = await exec(["up"], { env: { ...env, PATH: "/nonexistent", CLAUDECORD_TMUX_SOCKET: "" } });
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/tmux is required/);
  });
});
