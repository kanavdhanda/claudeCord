import { describe, expect, it } from "vitest";
import type { HubFrame } from "@claudecord/protocol";
import { Db } from "../src/db.js";
import { Hub, type NodeConn, type Outbound } from "../src/hub.js";

function setup() {
  const db = new Db(":memory:");
  const hub = new Hub(db);
  const posts: string[] = [];
  const notices: string[] = [];
  const out: Outbound = {
    ensureProject: async () => {},
    post: async (_p, a, text) => void posts.push(`${a.name}: ${text}`),
    postAsk: async () => {},
    postReport: async () => {},
    notice: async (_p, t) => void notices.push(t),
    refreshStatus: () => {},
  };
  hub.out = out;
  const sent: Record<string, HubFrame[]> = {};
  const conn = (name: string): NodeConn => {
    sent[name] = [];
    const c = { nodeName: name, send: (f: HubFrame) => void sent[name]!.push(f) };
    hub.nodeConnected(c);
    return c;
  };
  const reg = async (c: NodeConn, name: string, project = "alpha") =>
    hub.onNodeFrame(c, {
      t: "agent.register",
      cwd: "/x",
      agent: { agentId: `${project}/${name}`, name, project, adapter: "claude" },
    });
  return { hub, db, sent, conn, reg, posts, notices };
}

const delivers = (frames: HubFrame[] = []) => frames.filter((f) => f.t === "deliver" && f.from !== "system");

describe("hub routing", () => {
  it("sends unaddressed human messages to the lead only", async () => {
    const s = setup();
    const a = s.conn("mac");
    const b = s.conn("gpu");
    await s.reg(a, "otter");
    await s.reg(b, "heron");
    const targets = s.hub.humanMessage("alpha", "build the migration");
    expect(targets).toEqual(["otter"]);
    expect(delivers(s.sent.mac)).toHaveLength(1);
    expect(delivers(s.sent.gpu)).toHaveLength(0);
  });

  it("routes @mentions to the named agent", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    expect(s.hub.humanMessage("alpha", "@heron run the tests")).toEqual(["heron"]);
  });

  it("shows agent chat to peers and not to the sender", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.reg(s.conn("gpu"), "heron");
    await s.hub.onNodeFrame(a, { t: "agent.say", agentId: "alpha/otter", text: "schema looks fine" });
    expect(delivers(s.sent.gpu)).toHaveLength(1);
    expect(delivers(s.sent.mac)).toHaveLength(0);
    expect(s.posts).toEqual(["otter: schema looks fine"]);
  });

  it("does not forward agent messages that address the engineer", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.reg(s.conn("gpu"), "heron");
    await s.hub.onNodeFrame(a, { t: "agent.say", agentId: "alpha/otter", text: "@engineer which db?" });
    expect(delivers(s.sent.gpu)).toHaveLength(0);
  });

  it("stops agent to agent forwarding after a long streak and resets on human input", async () => {
    const s = setup();
    const a = s.conn("mac");
    const b = s.conn("gpu");
    await s.reg(a, "otter");
    await s.reg(b, "heron");
    for (let i = 0; i < 20; i++) await s.hub.onNodeFrame(a, { t: "agent.say", agentId: "alpha/otter", text: `m${i}` });
    expect(delivers(s.sent.gpu).length).toBe(11);
    expect(s.notices.some((n) => n.includes("Pausing forwarding"))).toBe(true);
    s.hub.humanMessage("alpha", "carry on");
    await s.hub.onNodeFrame(a, { t: "agent.say", agentId: "alpha/otter", text: "back" });
    expect(delivers(s.sent.gpu).length).toBe(12);
  });

  it("answers a pending question instead of delivering a new task", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.hub.onNodeFrame(a, { t: "agent.ask", agentId: "alpha/otter", askId: "q1", question: "postgres or sqlite?" });
    expect(s.hub.humanMessage("alpha", "postgres")).toEqual(["otter"]);
    const frames = s.sent.mac!;
    expect(frames.some((f) => f.t === "answer" && f.askId === "q1" && f.text === "postgres")).toBe(true);
    expect(s.hub.asks.get("alpha")).toEqual([]);
  });

  it("drops a stale question when the agent leaves waiting_input", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.hub.onNodeFrame(a, { t: "agent.ask", agentId: "alpha/otter", askId: "q1", question: "ok?" });
    await s.hub.onNodeFrame(a, { t: "agent.status", agentId: "alpha/otter", status: "thinking" });
    expect(s.hub.asks.get("alpha")).toEqual([]);
  });

  it("controls: killall, hold, stop reach the right node", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    expect(s.hub.killall("alpha")).toBe(2);
    expect(s.hub.hold(true, "alpha", "heron")).toBe(1);
    expect(s.sent.gpu!.some((f) => f.t === "hold" && f.on)).toBe(true);
    expect(s.sent.mac!.some((f) => f.t === "hold")).toBe(false);
    expect(s.hub.stop("alpha", "otter")).toBe(true);
  });

  it("makes the first agent lead and lets the lead change", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    expect(s.db.agentsOfProject("alpha").find((a) => a.is_lead)?.name).toBe("otter");
    s.db.setLead("alpha", "alpha/heron");
    expect(s.hub.humanMessage("alpha", "go")).toEqual(["heron"]);
  });
});

describe("tokens", () => {
  it("authenticates only active tokens", () => {
    const db = new Db(":memory:");
    const t = db.createToken("mac");
    expect(db.nodeForToken(t)).toBe("mac");
    expect(db.nodeForToken("bogus")).toBeNull();
    db.revokeToken("mac");
    expect(db.nodeForToken(t)).toBeNull();
  });
});
