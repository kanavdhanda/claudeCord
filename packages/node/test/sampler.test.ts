import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const tmux = vi.hoisted(() => ({
  calls: [] as string[][],
  screens: new Map<string, string | null>(),
  fail: false,
}));

vi.mock("../src/tmux.js", () => ({
  sampleMany: async (ids: string[]) => {
    tmux.calls.push(ids);
    if (tmux.fail) throw new Error("tmux exploded");
    return new Map(ids.map((id) => [id, tmux.screens.get(id) ?? null]));
  },
}));

import type { AgentRuntime } from "../src/agent.js";
import { Sampler } from "../src/sampler.js";

function fakeRuntime(paneId: string, dueAt = 0) {
  const seen: (string | null)[] = [];
  const rt = {
    paneId,
    dueAt,
    isStopped: false,
    seen,
    sample: vi.fn(async (screen: string | null) => {
      seen.push(screen);
      if (screen === null) rt.isStopped = true;
    }),
  };
  return rt as typeof rt & AgentRuntime;
}

beforeEach(() => {
  tmux.calls = [];
  tmux.screens = new Map();
  tmux.fail = false;
});
afterEach(() => vi.useRealTimers());

describe("Sampler", () => {
  it("samples every due agent with one batched call, however many there are", async () => {
    const s = new Sampler();
    const rts = Array.from({ length: 20 }, (_, i) => fakeRuntime(`%${i}`));
    for (const r of rts) {
      tmux.screens.set(r.paneId, `screen ${r.paneId}`);
      s.add(r);
    }
    await s.round();
    expect(tmux.calls).toHaveLength(1);
    expect(tmux.calls[0]).toHaveLength(20);
    for (const r of rts) expect(r.seen).toEqual([`screen ${r.paneId}`]);
  });

  it("skips agents that are not due yet, and makes no call when none are", async () => {
    const s = new Sampler();
    const now = 1_000_000;
    const later = fakeRuntime("%1", now + 5000);
    s.add(later);
    await s.round(now);
    expect(tmux.calls).toEqual([]);
    expect(later.sample).not.toHaveBeenCalled();
    const due = fakeRuntime("%2", now - 1);
    s.add(due);
    tmux.screens.set("%2", "x");
    await s.round(now);
    expect(tmux.calls).toEqual([["%2"]]);
  });

  it("tells a runtime its pane is gone with null, then stops tracking it", async () => {
    const s = new Sampler();
    const r = fakeRuntime("%9");
    s.add(r);
    await s.round();
    expect(r.seen).toEqual([null]);
    await s.round();
    expect(tmux.calls).toHaveLength(1);
  });

  it("stops watching an agent that was removed", async () => {
    const s = new Sampler();
    const r = fakeRuntime("%1");
    s.add(r);
    s.remove(r);
    await s.round();
    expect(tmux.calls).toEqual([]);
  });

  it("does not start a second pass while one is running", async () => {
    const s = new Sampler();
    s.add(fakeRuntime("%1"));
    await Promise.all([s.round(), s.round(), s.round()]);
    expect(tmux.calls).toHaveLength(1);
  });

  it("survives tmux failing and carries on the next pass", async () => {
    const s = new Sampler();
    const r = fakeRuntime("%1");
    s.add(r);
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    tmux.fail = true;
    await expect(s.round()).resolves.toBeUndefined();
    expect(err).toHaveBeenCalled();
    tmux.fail = false;
    tmux.screens.set("%1", "back");
    await s.round();
    expect(r.seen).toEqual(["back"]);
    err.mockRestore();
  });

  it("runs on its own once started, and stops when asked", async () => {
    vi.useFakeTimers();
    const s = new Sampler();
    const r = fakeRuntime("%1");
    tmux.screens.set("%1", "hello");
    s.add(r);
    s.start();
    s.start();
    await vi.advanceTimersByTimeAsync(300);
    expect(r.seen.length).toBeGreaterThanOrEqual(1);
    s.stop();
    const count = tmux.calls.length;
    r.dueAt = 0;
    await vi.advanceTimersByTimeAsync(2000);
    expect(tmux.calls.length).toBe(count);
  });
});
