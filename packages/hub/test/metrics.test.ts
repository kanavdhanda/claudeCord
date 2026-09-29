import { describe, expect, it } from "vitest";
import { ACCEPT_BOUNDS, Metrics } from "../src/metrics.js";
import { Db } from "../src/db.js";
import { Hub, type Outbound } from "../src/hub.js";

const MIN = 60_000;
const clock = (start = 10_000 * MIN) => {
  let t = start;
  return { now: () => t, advance: (ms: number) => void (t += ms) };
};

describe("Metrics", () => {
  it("counts events into one-minute buckets and sums a range", () => {
    const c = clock();
    const m = new Metrics(c.now);
    m.inc("msg_human");
    m.inc("msg_human", 2);
    c.advance(MIN);
    m.inc("msg_human");
    const r = m.insights(60, 1, ["msg_human"]);
    expect(r.totals.msg_human).toBe(4);
    expect(r.series.msg_human!.at(-1)).toBe(1);
    expect(r.series.msg_human!.at(-2)).toBe(3);
    expect(r.series.msg_human).toHaveLength(60);
  });

  it("groups minutes into larger buckets for a longer range", () => {
    const c = clock();
    const m = new Metrics(c.now);
    for (let i = 0; i < 30; i++) {
      m.inc("msg_agent");
      c.advance(MIN);
    }
    const r = m.insights(60, 15, ["msg_agent"]);
    expect(r.series.msg_agent).toHaveLength(4);
    expect(r.series.msg_agent!.reduce((a, b) => a + b, 0)).toBe(30);
    // The window ends one minute after the last event, so the last bucket holds 14 and the first is empty.
    expect(r.series.msg_agent![0]).toBe(0);
    expect(r.series.msg_agent!.at(-1)).toBe(14);
  });

  it("forgets what is older than a day, and reuses the slot cleanly", () => {
    const c = clock();
    const m = new Metrics(c.now, 1440);
    m.inc("x", 5);
    c.advance(1440 * MIN);
    expect(m.insights(1440, 60, ["x"]).totals.x).toBe(0);
    m.inc("x");
    expect(m.insights(1440, 60, ["x"]).totals.x).toBe(1);
  });

  it("only reports what is inside the asked range", () => {
    const c = clock();
    const m = new Metrics(c.now);
    m.inc("x", 7);
    c.advance(90 * MIN);
    m.inc("x", 3);
    expect(m.insights(60, 1, ["x"]).totals.x).toBe(3);
    expect(m.insights(180, 1, ["x"]).totals.x).toBe(10);
  });

  it("reports zeros for a series that never happened, without failing", () => {
    const r = new Metrics(clock().now).insights(60, 10, ["nothing"]);
    expect(r.series.nothing).toEqual([0, 0, 0, 0, 0, 0]);
    expect(r.acceptance).toEqual({ n: 0, p50Ms: null, p95Ms: null, buckets: [0, 0, 0, 0, 0] });
    expect(r.taskCycle).toEqual({ n: 0, avgMs: null });
  });

  it("sorts acceptance times into buckets and reports percentiles", () => {
    const m = new Metrics(clock().now);
    for (const ms of [200, 400, 900, 2000, 3000, 4000, 8000, 20_000, 90_000, 500]) m.accepted(ms);
    const a = m.insights(60, 60, []).acceptance;
    expect(a.n).toBe(10);
    expect(a.buckets).toEqual([4, 3, 1, 1, 1]);
    expect(a.p50Ms).toBe(ACCEPT_BOUNDS[1]);
    expect(a.p95Ms).toBe(60_000);
  });

  it("averages how long tasks take from assignment to done", () => {
    const m = new Metrics(clock().now);
    m.taskFinished(60_000);
    m.taskFinished(120_000);
    const r = m.insights(60, 60, []);
    expect(r.taskCycle).toEqual({ n: 2, avgMs: 90_000 });
    expect(r.totals.task_done).toBe(2);
  });

  it("measures how long each agent spends working, including a stretch still in progress", () => {
    const c = clock();
    const m = new Metrics(c.now);
    m.status("a", "idle");
    m.status("a", "thinking");
    c.advance(30_000);
    m.status("a", "executing");
    c.advance(10_000);
    m.status("a", "idle");
    c.advance(5000);
    m.status("b", "thinking");
    c.advance(70_000);
    expect(m.busiest()).toEqual([
      { agentId: "b", busyMs: 70_000 },
      { agentId: "a", busyMs: 40_000 },
    ]);
    m.forget("b");
    expect(m.busiest().map((x) => x.agentId)).toEqual(["a"]);
    expect(m.busiest(0)).toEqual([]);
  });

  it("saves only the non-zero buckets and restores them", () => {
    const c = clock();
    const a = new Metrics(c.now);
    a.inc("msg_human", 3);
    c.advance(5 * MIN);
    a.inc("msg_agent", 2);
    const saved = JSON.parse(JSON.stringify(a.toJSON()));
    expect(Object.keys(saved).sort()).toEqual(["msg_agent", "msg_human"]);
    expect(Object.keys(saved.msg_human)).toHaveLength(1);
    const b = new Metrics(c.now);
    b.load(saved);
    expect(b.insights(60, 1, []).totals).toEqual({ msg_human: 3, msg_agent: 2 });
  });

  it("ignores saved data that is old, malformed or from the future", () => {
    const c = clock();
    const m = new Metrics(c.now);
    const nowMin = Math.floor(c.now() / MIN);
    m.load({
      ok: { [nowMin]: 4 },
      old: { [nowMin - 5000]: 9 },
      future: { [nowMin + 10]: 9 },
      bad: { x: 1, [nowMin]: "nope", [nowMin - 1]: NaN },
      notAnObject: 5,
    });
    m.load(null);
    m.load("string");
    const t = m.insights(1440, 60, []).totals;
    expect(t).toEqual({ ok: 4 });
  });
});

describe("hub instrumentation", () => {
  function hubWith(metrics = new Metrics()) {
    const db = new Db(":memory:");
    const hub = new Hub(db, 50, 60_000, metrics);
    hub.out = {
      ensureProject: async () => {},
      post: async () => {},
      postAsk: async () => {},
      postReport: async () => {},
      postFile: async () => {},
      confirm: async () => {},
      notice: async () => {},
      refreshStatus: () => {},
    } satisfies Outbound;
    const sent: unknown[] = [];
    const conn = (name: string) => {
      const c = { nodeName: name, send: (f: unknown) => void sent.push(f) };
      hub.nodeConnected(c as never);
      return c as never;
    };
    const reg = (c: never, project: string, name: string) =>
      hub.onNodeFrame(c, {
        t: "agent.register",
        cwd: "/x",
        agent: { agentId: `${project}/${name}`, name, project, adapter: "claude" },
      });
    return { hub, metrics, conn, reg, sent };
  }
  const totals = (m: Metrics) => m.insights(60, 60, []).totals;

  it("counts joins, human messages, agent chat and acceptance", async () => {
    const { hub, metrics, conn, reg, sent } = hubWith();
    const a = conn("mac");
    await reg(a, "p", "otter");
    hub.humanMessage("p", "go", undefined, "c:1");
    await hub.onNodeFrame(a, { t: "agent.say", agentId: "p/otter", text: "on it" });
    const d = (sent as { msgId?: string; from?: string }[]).find((f) => f.from === "engineer")!;
    await hub.onNodeFrame(a, { t: "agent.accepted", agentId: "p/otter", msgIds: [d.msgId!] });
    const t = totals(metrics);
    expect(t.agent_joined).toBe(1);
    expect(t.msg_human).toBe(1);
    expect(t.msg_agent).toBe(1);
    expect(metrics.insights(60, 60, []).acceptance.n).toBe(1);
  });

  it("does not count a human message nobody received", () => {
    const { hub, metrics } = hubWith();
    hub.humanMessage("nobody", "hello?");
    expect(totals(metrics).msg_human).toBeUndefined();
  });

  it("counts the task lifecycle and how long it took", async () => {
    const { hub, metrics, conn, reg, sent } = hubWith();
    const lead = conn("mac");
    const worker = conn("gpu");
    await reg(lead, "p", "otter");
    await reg(worker, "p", "heron");
    await hub.onNodeFrame(lead, { t: "agent.assign", agentId: "p/otter", to: "heron", task: "x" });
    const d = (sent as { msgId?: string; text?: string }[]).find((f) => f.text?.startsWith("Task T1"))!;
    await hub.onNodeFrame(worker, { t: "agent.accepted", agentId: "p/heron", msgIds: [d.msgId!] });
    await hub.onNodeFrame(worker, { t: "agent.taskdone", agentId: "p/heron", taskId: "T1", summary: "done" });
    const t = totals(metrics);
    expect([t.task_assigned, t.task_accepted, t.task_done]).toEqual([1, 1, 1]);
    expect(metrics.insights(60, 60, []).taskCycle.n).toBe(1);
  });

  it("counts limits, redactions, blocked files and files sent", async () => {
    const { hub, metrics, conn, reg } = hubWith();
    const a = conn("mac");
    await reg(a, "p", "otter");
    await reg(conn("gpu"), "p", "heron");
    await hub.onNodeFrame(a, { t: "agent.limit", agentId: "p/otter", kind: "session" });
    await hub.onNodeFrame(a, { t: "agent.say", agentId: "p/otter", text: `token ghp_${"a".repeat(36)}` });
    const key = Buffer.from("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----").toString("base64");
    await hub.onNodeFrame(a, {
      t: "file.chunk",
      transferId: "t1",
      agentId: "p/otter",
      name: "k.txt",
      seq: 0,
      last: true,
      data: key,
    });
    await hub.onNodeFrame(a, {
      t: "file.chunk",
      transferId: "t2",
      agentId: "p/otter",
      name: "ok.txt",
      seq: 0,
      last: true,
      data: Buffer.from("hello").toString("base64"),
    });
    await hub.onNodeFrame(a, {
      t: "file.chunk",
      transferId: "t3",
      agentId: "p/otter",
      to: "heron",
      name: "p.txt",
      seq: 0,
      last: true,
      data: Buffer.from("peer data").toString("base64"),
    });
    const t = totals(metrics);
    expect(t.limits).toBe(1);
    expect(t.redactions).toBe(1);
    expect(t.secret_blocks).toBe(1);
    expect(t.file_count).toBe(2);
    expect(t.file_bytes).toBeGreaterThan(5);
  });

  it("tracks working time from status changes and forgets an agent that left", async () => {
    const c = clock();
    const { hub, metrics, conn, reg } = hubWith(new Metrics(c.now));
    const a = conn("mac");
    await reg(a, "p", "otter");
    await hub.onNodeFrame(a, { t: "agent.status", agentId: "p/otter", status: "thinking" });
    c.advance(5000);
    expect(metrics.busiest()).toEqual([{ agentId: "p/otter", busyMs: 5000 }]);
    await hub.onNodeFrame(a, { t: "agent.gone", agentId: "p/otter" });
    expect(metrics.busiest()).toEqual([]);
  });
});
