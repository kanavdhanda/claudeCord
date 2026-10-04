import { afterEach, describe, expect, it } from "vitest";
import { createServer, type Server } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { callDaemon, currentAgentId, meshDir, sockPath } from "../src/ipc.js";

let server: Server | undefined;
let dir: string | undefined;
const saved = { ...process.env };

afterEach(() => {
  server?.close();
  server = undefined;
  if (dir) rmSync(dir, { recursive: true, force: true });
  dir = undefined;
  for (const k of Object.keys(process.env)) if (!(k in saved)) delete process.env[k];
  Object.assign(process.env, saved);
});

/** A daemon stand-in that answers every request line with whatever the handler returns, or hangs up. */
function listen(handler: (line: string) => string | null): Promise<string> {
  dir = mkdtempSync(join(tmpdir(), "cc-ipc-"));
  const path = join(dir, "s.sock");
  process.env.CLAUDECORD_SOCK = path;
  return new Promise((resolve) => {
    server = createServer((sock) => {
      let buf = "";
      sock.on("data", (d) => {
        buf += d.toString();
        const i = buf.indexOf("\n");
        if (i < 0) return;
        const out = handler(buf.slice(0, i));
        if (out === null) sock.destroy();
        else sock.write(out + "\n");
      });
    }).listen(path, () => resolve(path));
  });
}

describe("paths", () => {
  it("honours the environment overrides and otherwise lives under the home directory", () => {
    process.env.CLAUDECORD_HOME = "/custom/home";
    delete process.env.CLAUDECORD_SOCK;
    expect(meshDir()).toBe("/custom/home");
    expect(sockPath()).toBe(join("/custom/home", "node.sock"));
    process.env.CLAUDECORD_SOCK = "/elsewhere/s.sock";
    expect(sockPath()).toBe("/elsewhere/s.sock");
  });
});

describe("callDaemon", () => {
  it("sends one JSON line and returns the parsed reply", async () => {
    let seen = "";
    await listen((line) => {
      seen = line;
      return JSON.stringify({ ok: true, data: "pong" });
    });
    const r = await callDaemon({ op: "ping" });
    expect(JSON.parse(seen)).toEqual({ op: "ping" });
    expect(r).toEqual({ ok: true, data: "pong" });
  });

  it("returns a failure reply as is", async () => {
    await listen(() => JSON.stringify({ ok: false, error: "unknown agent" }));
    expect(await callDaemon({ op: "ls" })).toEqual({ ok: false, error: "unknown agent" });
  });

  it("rejects when nothing is listening", async () => {
    process.env.CLAUDECORD_SOCK = join(tmpdir(), "definitely-not-here.sock");
    await expect(callDaemon({ op: "ping" })).rejects.toThrow();
  });

  it("rejects a reply that is not JSON", async () => {
    await listen(() => "not json at all");
    await expect(callDaemon({ op: "ping" })).rejects.toThrow();
  });

  it("rejects when the daemon hangs up without answering", async () => {
    await listen(() => null);
    await expect(callDaemon({ op: "ping" })).rejects.toThrow(/closed connection/);
  });

  it("gives up after the timeout", async () => {
    dir = mkdtempSync(join(tmpdir(), "cc-ipc-"));
    process.env.CLAUDECORD_SOCK = join(dir, "s.sock");
    server = createServer(() => {
      /* accept and never answer */
    }).listen(process.env.CLAUDECORD_SOCK);
    await new Promise((r) => server!.once("listening", r));
    await expect(callDaemon({ op: "ping" }, 150)).rejects.toThrow(/timeout/);
  });
});

describe("currentAgentId", () => {
  it("reads the agent id the daemon put in the environment, and refuses without one", () => {
    process.env.CLAUDECORD_AGENT_ID = "proj/otter";
    expect(currentAgentId()).toBe("proj/otter");
    delete process.env.CLAUDECORD_AGENT_ID;
    expect(() => currentAgentId()).toThrow(/CLAUDECORD_AGENT_ID/);
  });
});
