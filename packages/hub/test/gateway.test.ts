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
    ws.send(encode({ t: "agent.register", cwd: "/x", agent: { agentId: "p/a", name: "a", project: "p", adapter: "claude" } }));
    ws.send(encode({ t: "agent.say", agentId: "p/a", text: "hello" }));
    await new Promise((r) => setTimeout(r, 150));
    expect(posts).toContain("a: hello");
    ws.close();
  });
});
