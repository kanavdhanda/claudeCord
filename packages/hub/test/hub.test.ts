import { describe, expect, it, vi } from "vitest";
import type { HubFrame } from "@claudecord/protocol";
import { Db } from "../src/db.js";
import { Hub, type NodeConn, type Outbound } from "../src/hub.js";

function setup() {
  const db = new Db(":memory:");
  const hub = new Hub(db, 12);
  const posts: string[] = [];
  const notices: string[] = [];
  const out: Outbound = {
    ensureProject: async () => {},
    post: async (_p, a, text) => void posts.push(`${a.name}: ${text}`),
    postAsk: async () => {},
    postReport: async () => {},
    postFile: async () => {},
  confirm: async () => {},
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
    const { targets } = s.hub.humanMessage("alpha", "build the migration");
    expect(targets).toEqual(["otter"]);
    expect(delivers(s.sent.mac)).toHaveLength(1);
    expect(delivers(s.sent.gpu)).toHaveLength(0);
  });

  it("routes @mentions to the named agent", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    expect(s.hub.humanMessage("alpha", "@heron run the tests").targets).toEqual(["heron"]);
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
    expect(s.hub.humanMessage("alpha", "postgres").targets).toEqual(["otter"]);
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
    expect(s.hub.humanMessage("alpha", "go").targets).toEqual(["heron"]);
  });
});

describe("acceptance and task assignment", () => {
  const systemTexts = (frames: HubFrame[] = []) => frames.filter((f) => f.t === "deliver" && f.from === "system").map((f) => (f as { text: string }).text);
  const lastDeliver = (frames: HubFrame[] = []) => [...frames].reverse().find((f) => f.t === "deliver" && f.from !== "system") as Extract<HubFrame, { t: "deliver" }> | undefined;

  it("confirms a human message once the agent reports acceptance", async () => {
    const s = setup();
    const confirmed: string[] = [];
    s.hub.out.confirm = async (_p, ref, name) => void confirmed.push(`${name}:${ref}`);
    const a = s.conn("mac");
    await s.reg(a, "otter");
    s.hub.humanMessage("alpha", "build it", undefined, "c1:m1");
    const d = lastDeliver(s.sent.mac)!;
    expect(d.msgId).toBeTruthy();
    expect(confirmed).toEqual([]);
    await s.hub.onNodeFrame(a, { t: "agent.accepted", agentId: "alpha/otter", msgIds: [d.msgId!] });
    expect(confirmed).toEqual(["otter:c1:m1"]);
    // A second report for the same delivery does nothing.
    await s.hub.onNodeFrame(a, { t: "agent.accepted", agentId: "alpha/otter", msgIds: [d.msgId!] });
    expect(confirmed).toHaveLength(1);
  });

  it("ignores acceptance claimed by a different agent", async () => {
    const s = setup();
    const confirmed: string[] = [];
    s.hub.out.confirm = async (_p, ref) => void confirmed.push(ref);
    const a = s.conn("mac");
    const b = s.conn("gpu");
    await s.reg(a, "otter");
    await s.reg(b, "heron");
    s.hub.humanMessage("alpha", "go", undefined, "c1:m1");
    const d = lastDeliver(s.sent.mac)!;
    await s.hub.onNodeFrame(b, { t: "agent.accepted", agentId: "alpha/heron", msgIds: [d.msgId!] });
    expect(confirmed).toEqual([]);
  });

  it("tells the human when the target is held or offline", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.hub.onNodeFrame(a, { t: "agent.status", agentId: "alpha/otter", status: "paused" });
    expect(s.hub.humanMessage("alpha", "go", undefined, "c:1").held).toEqual([{ name: "otter", why: "paused" }]);
    s.hub.nodeDisconnected(a);
    expect(s.hub.humanMessage("alpha", "go", undefined, "c:2").offline).toEqual(["otter"]);
  });

  it("posts a notice if the agent has not accepted within the timeout", async () => {
    vi.useFakeTimers();
    try {
      const s = setup();
      await s.reg(s.conn("mac"), "otter");
      s.hub.humanMessage("alpha", "go", undefined, "c:1");
      await vi.advanceTimersByTimeAsync(31_000);
      expect(s.notices.some((n) => n.includes("has not picked up"))).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not post the timeout notice when the agent accepted in time", async () => {
    vi.useFakeTimers();
    try {
      const s = setup();
      const a = s.conn("mac");
      await s.reg(a, "otter");
      s.hub.humanMessage("alpha", "go", undefined, "c:1");
      await s.hub.onNodeFrame(a, { t: "agent.accepted", agentId: "alpha/otter", msgIds: [lastDeliver(s.sent.mac)!.msgId!] });
      await vi.advanceTimersByTimeAsync(31_000);
      expect(s.notices.some((n) => n.includes("has not picked up"))).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("briefs the lead and workers, and tells the lead about new peers", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    expect(systemTexts(s.sent.mac).join("\n")).toMatch(/lead of project alpha and nobody else/);
    await s.reg(s.conn("gpu"), "heron");
    expect(systemTexts(s.sent.gpu).join("\n")).toMatch(/otter is the lead/);
    expect(systemTexts(s.sent.mac).join("\n")).toMatch(/heron joined/);
  });

  it("runs the full task flow: assign, accept, done, all done", async () => {
    const s = setup();
    const lead = s.conn("mac");
    const worker = s.conn("gpu");
    await s.reg(lead, "otter");
    await s.reg(worker, "heron");
    await s.hub.onNodeFrame(lead, { t: "agent.assign", agentId: "alpha/otter", to: "heron", task: "write the migration" });
    const d = lastDeliver(s.sent.gpu)!;
    expect(d.from).toBe("otter");
    expect(d.text).toContain("Task T1: write the migration");
    expect(s.posts.some((p) => p.includes("@heron T1: write the migration"))).toBe(true);
    expect(s.db.getTask("alpha", "T1")?.state).toBe("assigned");

    await s.hub.onNodeFrame(worker, { t: "agent.accepted", agentId: "alpha/heron", msgIds: [d.msgId!] });
    expect(s.db.getTask("alpha", "T1")?.state).toBe("accepted");
    expect(s.notices).toContain("heron accepted T1.");

    await s.hub.onNodeFrame(worker, { t: "agent.taskdone", agentId: "alpha/heron", taskId: "T1", summary: "migration written" });
    expect(s.db.getTask("alpha", "T1")).toMatchObject({ state: "done", summary: "migration written" });
    const toLead = systemTexts(s.sent.mac).join("\n");
    expect(toLead).toMatch(/heron finished T1: migration written/);
    expect(toLead).toMatch(/All 1 task\(s\) are done/);
  });

  it("only lets the lead assign, and only to real peers", async () => {
    const s = setup();
    const lead = s.conn("mac");
    const worker = s.conn("gpu");
    await s.reg(lead, "otter");
    await s.reg(worker, "heron");
    await s.hub.onNodeFrame(worker, { t: "agent.assign", agentId: "alpha/heron", to: "otter", task: "x" });
    expect(systemTexts(s.sent.gpu).join("\n")).toMatch(/Only the lead assigns/);
    expect(s.db.tasksOfProject("alpha")).toHaveLength(0);
    await s.hub.onNodeFrame(lead, { t: "agent.assign", agentId: "alpha/otter", to: "ghost", task: "x" });
    expect(systemTexts(s.sent.mac).join("\n")).toMatch(/Cannot assign to ghost/);
    expect(s.db.tasksOfProject("alpha")).toHaveLength(0);
  });

  it("rejects task_done from an agent that does not own the task", async () => {
    const s = setup();
    const lead = s.conn("mac");
    await s.reg(lead, "otter");
    await s.reg(s.conn("gpu"), "heron");
    const third = s.conn("cpu");
    await s.reg(third, "finch");
    await s.hub.onNodeFrame(lead, { t: "agent.assign", agentId: "alpha/otter", to: "heron", task: "x" });
    await s.hub.onNodeFrame(third, { t: "agent.taskdone", agentId: "alpha/finch", taskId: "T1", summary: "nope" });
    expect(s.db.getTask("alpha", "T1")?.state).toBe("assigned");
    expect(systemTexts(s.sent.cpu).join("\n")).toMatch(/not assigned to you/);
  });

  it("moves the lead role and re-briefs everyone", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    s.hub.setLead("alpha", "alpha/heron");
    expect(systemTexts(s.sent.gpu).at(-1)).toMatch(/You are the lead/);
    expect(systemTexts(s.sent.mac).at(-1)).toMatch(/heron is the lead/);
  });
});

describe("file transfer", () => {
  const chunk = (agentId: string, o: Partial<Extract<import("@claudecord/protocol").NodeFrame, { t: "file.chunk" }>> = {}) => ({
    t: "file.chunk" as const, transferId: "t1", agentId, name: "a.txt", seq: 0, last: true,
    data: Buffer.from("hello").toString("base64"), ...o,
  });

  it("relays a peer transfer chunk by chunk to the right node", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.reg(s.conn("gpu"), "heron");
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { to: "heron", seq: 0, last: false }));
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { to: "heron", seq: 1, last: true }));
    const got = s.sent.gpu!.filter((f) => f.t === "file.chunk");
    expect(got).toHaveLength(2);
    expect(got[0]).toMatchObject({ agentId: "alpha/heron", from: "otter", name: "a.txt" });
    expect(s.sent.mac!.some((f) => f.t === "file.chunk")).toBe(false);
  });

  it("posts a transfer without a recipient to Discord once, reassembled", async () => {
    const s = setup();
    const files: string[] = [];
    s.hub.out.postFile = async (_p, _a, name, data) => void files.push(`${name}:${data.toString()}`);
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { seq: 0, last: false, data: Buffer.from("hel").toString("base64") }));
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { seq: 1, last: true, data: Buffer.from("lo").toString("base64") }));
    expect(files).toEqual(["a.txt:hello"]);
  });

  it("rejects oversize uploads to Discord", async () => {
    const s = setup();
    const files: string[] = [];
    s.hub.out.postFile = async (_p, _a, name) => void files.push(name);
    const a = s.conn("mac");
    await s.reg(a, "otter");
    const big = Buffer.alloc(6 * 1024 * 1024).toString("base64");
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { seq: 0, last: false, data: big }));
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { seq: 1, last: true, data: big }));
    expect(files).toEqual([]);
    expect(s.notices.some((n) => n.includes("limit"))).toBe(true);
  });

  it("tells the room when the recipient does not exist", async () => {
    const s = setup();
    const a = s.conn("mac");
    await s.reg(a, "otter");
    await s.hub.onNodeFrame(a, chunk("alpha/otter", { to: "ghost" }));
    expect(s.notices.some((n) => n.includes("ghost"))).toBe(true);
  });

  it("splits a human file into ordered chunks for the addressed agent only", async () => {
    const s = setup();
    await s.reg(s.conn("mac"), "otter");
    await s.reg(s.conn("gpu"), "heron");
    const data = Buffer.alloc(500 * 1024, 7);
    expect(s.hub.sendFile("alpha", "@heron data set", "d.bin", data)).toEqual(["heron"]);
    const got = s.sent.gpu!.filter((f) => f.t === "file.chunk") as Extract<HubFrame, { t: "file.chunk" }>[];
    expect(got.map((c) => c.seq)).toEqual([0, 1, 2]);
    expect(got.map((c) => c.last)).toEqual([false, false, true]);
    expect(Buffer.concat(got.map((c) => Buffer.from(c.data, "base64"))).equals(data)).toBe(true);
    expect(s.sent.mac!.some((f) => f.t === "file.chunk")).toBe(false);
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
