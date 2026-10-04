import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { Server } from "node:http";
import type { AddressInfo } from "node:net";
import WebSocket from "ws";
import { NODE_CONNECT_PATH, encode, parseHubFrame, type HubFrame } from "@claudecord/protocol";
import { Db } from "../src/db.js";
import { startGateway } from "../src/gateway.js";
import { Hub, type Outbound } from "../src/hub.js";

let server: Server;
let url: string;
let token: string;
const posts: string[] = [];

beforeAll(async () => {
  const db = new Db(":memory:");
  token = db.createToken("mac");
  const hub = new Hub(db);
  hub.out = {
    ensureProject: async () => {},
    post: async (_p, a, t) => void posts.push(`${a.name}: ${t}`),
    postAsk: async () => {},
    postReport: async () => {},
    postFile: async () => {},
    confirm: async () => {},
    notice: async () => {},
    refreshStatus: () => {},
  } satisfies Outbound;
  server = startGateway(hub, 0);
  await new Promise((r) => server.once("listening", r));
  url = `ws://127.0.0.1:${(server.address() as AddressInfo).port}${NODE_CONNECT_PATH}`;
});

afterAll(() => void server.close());

const open = (auth?: string) =>
  new Promise<{ ws: WebSocket; ok: boolean; frames: (HubFrame | null)[] }>((resolve) => {
    const ws = new WebSocket(url, auth ? { headers: { Authorization: auth } } : undefined);
    const frames: (HubFrame | null)[] = [];
    ws.on("message", (d) => frames.push(parseHubFrame(d.toString())));
    ws.on("open", () => resolve({ ws, ok: true, frames }));
    ws.on("error", () => resolve({ ws, ok: false, frames }));
    ws.on("unexpected-response", () => resolve({ ws, ok: false, frames }));
  });

describe("node gateway", () => {
  it("rejects missing and bad tokens", async () => {
    expect((await open()).ok).toBe(false);
    expect((await open("Bearer nope")).ok).toBe(false);
  });

  it("accepts a valid token, welcomes, and relays agent chat", async () => {
    const { ws, ok, frames } = await open(`Bearer ${token}`);
    expect(ok).toBe(true);
    await new Promise((r) => setTimeout(r, 50));
    expect(frames[0]).toEqual({ t: "welcome", nodeId: "mac" });
    ws.send(
      encode({ t: "agent.register", cwd: "/x", agent: { agentId: "p/a", name: "a", project: "p", adapter: "claude" } }),
    );
    ws.send(encode({ t: "agent.say", agentId: "p/a", text: "hello" }));
    await new Promise((r) => setTimeout(r, 150));
    expect(posts).toContain("a: hello");
    ws.close();
  });
});

describe("gateway limits", () => {
  const closed = (ws: WebSocket) => new Promise<number>((r) => ws.once("close", (code) => r(code)));

  it("closes a connection that sends a frame over the size limit", async () => {
    const { ws, ok } = await open(`Bearer ${token}`);
    expect(ok).toBe(true);
    const done = closed(ws);
    ws.send("x".repeat(2 * 1024 * 1024));
    expect(await done).toBe(1009); // message too big
  });

  it("closes a connection that floods frames faster than the rate limit", async () => {
    const { ws, ok } = await open(`Bearer ${token}`);
    expect(ok).toBe(true);
    const done = closed(ws);
    const frame = encode({ t: "hello", nodeName: "mac", version: "t" });
    for (let i = 0; i < 2000; i++) ws.send(frame);
    expect(await done).toBe(1008); // policy violation
  });

  it("replies with an error to a malformed frame and stays connected", async () => {
    const { ws, ok, frames } = await open(`Bearer ${token}`);
    expect(ok).toBe(true);
    ws.send(JSON.stringify({ t: "agent.say", agentId: "p/a", text: "x".repeat(9000) }));
    await new Promise((r) => setTimeout(r, 100));
    expect(frames.some((f) => f?.t === "error")).toBe(true);
    expect(ws.readyState).toBe(ws.OPEN);
    ws.close();
  });
});

describe("gateway under abuse", () => {
  it("answers 429 to an address that keeps sending bad tokens, even when a later token is valid", async () => {
    let refused = 0;
    for (let i = 0; i < 25; i++) if (!(await open(`Bearer bad${i}`)).ok) refused++;
    expect(refused).toBe(25);
    // The address is now blocked, so even the real token is turned away until the window passes.
    expect((await open(`Bearer ${token}`)).ok).toBe(false);
  });
});
