import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Server } from "node:http";
import type { AddressInfo } from "node:net";
import { Auth } from "../../../hub/src/auth.js";
import { Db } from "../../../hub/src/db.js";
import { startGateway } from "../../../hub/src/gateway.js";
import { Hub, type Outbound } from "../../../hub/src/hub.js";
import { createHttpHandler } from "../../../hub/src/http.js";

export const ROOT = fileURLToPath(new URL("../../../..", import.meta.url));
export const CLI = join(ROOT, "packages/node/dist/cli.js");
export const IPC = join(ROOT, "packages/agent-tools/dist/ipc.js");
export const FAKE = fileURLToPath(new URL("../fixtures/fake-claude.mjs", import.meta.url));

/** True when real tmux is available and the workspace has been built, which the end-to-end tests need. */
export const e2eReady = process.platform !== "win32" && spawnSync("tmux", ["-V"]).status === 0 && existsSync(CLI) && existsSync(IPC);

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function waitFor<T>(what: string, fn: () => T | undefined | false, ms = 25_000, diagnostics: () => string = () => ""): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = fn();
    if (v) return v as T;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}\n${diagnostics()}`);
    await sleep(150);
  }
}

/** A hub with the real router and gateway, and a Discord layer that just records what it was asked to do. */
export async function startRecordingHub(opts: { streakLimit?: number; acceptTimeoutMs?: number } = {}) {
  const posts: string[] = [];
  const notices: string[] = [];
  const confirms: string[] = [];
  const files: string[] = [];
  const db = new Db(":memory:");
  const hub = new Hub(db, opts.streakLimit ?? 50, opts.acceptTimeoutMs ?? 60_000);
  hub.out = {
    ensureProject: async () => {},
    post: async (_p, a, t) => void posts.push(`${a.name}: ${t}`),
    postAsk: async () => {},
    postReport: async () => {},
    postFile: async (_p, a, name) => void files.push(`${a.name}:${name}`),
    confirm: async (_p, ref, name) => void confirms.push(`${name}:${ref}`),
    notice: async (_p, t) => void notices.push(t),
    refreshStatus: () => {},
  } satisfies Outbound;
  const auth = new Auth(db);
  let port = 0;
  // The HTTP side (pairing, dashboard) is on the same server and port as the node gateway, like the real hub.
  const server: Server = startGateway(hub, 0, createHttpHandler({ hub, auth, publicUrl: () => `http://127.0.0.1:${port}` }));
  await new Promise((r) => server.once("listening", r));
  port = (server.address() as AddressInfo).port;
  return { hub, db, auth, port, posts, notices, confirms, files, token: db.createToken("e2e"), close: () => void server.close() };
}

/** Puts a stand-in `claude` first on PATH. Returns the directory so it can be added to PATH. */
export function installFakeClaude(tmp: string): string {
  const bin = join(tmp, "bin");
  mkdirSync(bin, { recursive: true });
  copyFileSync(FAKE, join(bin, "claude"));
  chmodSync(join(bin, "claude"), 0o755);
  return bin;
}

export function makeTmp(): string {
  return mkdtempSync(join(tmpdir(), "cc-e2e-"));
}

/** Screens of every pane on a tmux socket, for failure messages. */
export function paneDump(sock: string): string {
  const panes = spawnSync("tmux", ["-L", sock, "list-panes", "-a", "-F", "#{pane_id}"], { encoding: "utf8" }).stdout.split("\n").filter(Boolean);
  return panes.map((p) => `--- pane ${p}\n${spawnSync("tmux", ["-L", sock, "capture-pane", "-p", "-t", p], { encoding: "utf8" }).stdout}`).join("\n") || "(no panes)";
}

export function readIf(path: string): string {
  return existsSync(path) ? readFileSync(path, "utf8") : "";
}
