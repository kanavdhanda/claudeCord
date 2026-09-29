import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const tmux = vi.hoisted(() => ({
  pasted: [] as string[],
  keys: [] as string[][],
  killed: [] as string[],
}));

vi.mock("../src/tmux.js", () => ({
  pasteAndSubmit: async (_p: string, text: string) => void tmux.pasted.push(text),
  sendKeys: async (_p: string, keys: string[]) => void tmux.keys.push(keys),
  killPane: async (id: string) => void tmux.killed.push(id),
}));

import { claude } from "../src/adapters/claude.js";
import {
  AgentRuntime,
  COLD_AFTER_MS,
  COLD_MS,
  HOT_MS,
  WARM_AFTER_MS,
  WARM_MS,
  alignedDue,
  type AgentEvents,
} from "../src/agent.js";

const IDLE = '\n │ > Try "fix the tests"\n ? for shortcuts\n';
const BUSY = "\n ✽ Thinking… (3s · esc to interrupt)\n ? for shortcuts\n";
const PERMISSION = `
 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again
   3. No, and tell Claude what to do differently
 Esc to cancel
`;
const TRUST = `
 Do you trust the files in this folder?
 ❯ 1. Yes, proceed
   2. No, exit
 Enter to confirm · Esc to cancel
`;

function make(firstMessage?: string) {
  const ev = {
    status: vi.fn(),
    ask: vi.fn(),
    limit: vi.fn(),
    gone: vi.fn(),
    accepted: vi.fn(),
  } satisfies AgentEvents;
  const spec = { agentId: "p/a", name: "a", project: "p", adapter: "claude" as const };
  const rt = new AgentRuntime(spec, "%1", "/tmp/p", claude, ev, firstMessage);
  return { rt, ev };
}

/** Shows the runtime a screen, then lets time pass the way the sampler would between looks. */
async function look(rt: AgentRuntime, screen: string | null, advanceMs = 900) {
  await rt.sample(screen);
  vi.advanceTimersByTime(advanceMs);
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
  tmux.pasted = [];
  tmux.keys = [];
  tmux.killed = [];
});
afterEach(() => vi.useRealTimers());

describe("delivery and acceptance", () => {
  it("delivers queued messages when idle and confirms acceptance once the agent is busy", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "build the migration", msgId: "m1" });
    await look(rt, IDLE);
    expect(tmux.pasted).toEqual(["[engineer] build the migration"]);
    expect(ev.accepted).not.toHaveBeenCalled();
    await look(rt, BUSY);
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
    await look(rt, BUSY);
    expect(ev.accepted).toHaveBeenCalledTimes(1);
  });

  it("recognises acceptance from the echoed text when the turn finishes between looks", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "rename the helper function", msgId: "m1" });
    await look(rt, IDLE);
    await look(rt, `${IDLE}\n > [engineer] rename the helper function\n`);
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
  });

  it("does not claim acceptance while the pane just sits idle", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "do something", msgId: "m1" });
    await look(rt, IDLE);
    for (let i = 0; i < 8; i++) await look(rt, IDLE, 1000);
    expect(ev.accepted).not.toHaveBeenCalled();
  });

  it("treats the agent speaking as acceptance", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "do something", msgId: "m1" });
    await look(rt, IDLE);
    rt.noteActivity();
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
  });

  it("batches waiting messages into one injection and accepts them together", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.setHeld(true);
    rt.enqueue({ from: "engineer", text: "first", msgId: "m1" });
    rt.enqueue({ from: "otter", text: "second", msgId: "m2" });
    for (let i = 0; i < 4; i++) await look(rt, IDLE, 1000);
    expect(tmux.pasted).toEqual([]);
    rt.setHeld(false);
    await look(rt, IDLE);
    expect(tmux.pasted).toEqual(["[engineer] first\n\n[otter] second"]);
    await look(rt, BUSY);
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1", "m2"]);
  });

  it("does not inject while the agent is busy, and injects once it is idle again", async () => {
    const { rt } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "later", msgId: "m1" });
    for (let i = 0; i < 4; i++) await look(rt, BUSY, 1000);
    expect(tmux.pasted).toEqual([]);
    await look(rt, IDLE);
    expect(tmux.pasted).toHaveLength(1);
  });

  it("waits out the cooldown between injections", async () => {
    const { rt } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "one", msgId: "m1" });
    await look(rt, IDLE, 100);
    expect(tmux.pasted).toHaveLength(1);
    rt.enqueue({ from: "engineer", text: "two", msgId: "m2" });
    await look(rt, IDLE, 100);
    expect(tmux.pasted).toHaveLength(1);
    await look(rt, IDLE, 3000);
    await look(rt, IDLE);
    expect(tmux.pasted).toHaveLength(2);
  });

  it("sends the team rules as the first message when the agent has no flag for them", async () => {
    const { rt } = make("You are a, an engineer.");
    await look(rt, IDLE);
    expect(tmux.pasted).toEqual(["You are a, an engineer."]);
    await look(rt, IDLE);
    expect(tmux.pasted).toHaveLength(1);
  });
});

describe("prompts and limits", () => {
  it("answers the folder trust dialog by itself at startup", async () => {
    const { rt } = make();
    // After answering it pauses for the dialog to close, which takes fake time here.
    const sampling = rt.sample(TRUST);
    await vi.advanceTimersByTimeAsync(900);
    await sampling;
    expect(tmux.keys[0]).toEqual(["Enter"]);
  });

  it("relays a permission prompt once and maps the human's reply onto the menu", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    await look(rt, PERMISSION);
    await look(rt, PERMISSION);
    expect(ev.ask).toHaveBeenCalledTimes(1);
    const [, askId, question, options] = ev.ask.mock.calls[0]!;
    expect(question).toContain("Do you want to proceed?");
    expect(options).toHaveLength(3);
    expect(ev.status).toHaveBeenLastCalledWith("p/a", "waiting_input", undefined);
    expect(await rt.answerPrompt(askId as string, "2")).toBe(true);
    expect(tmux.keys.at(-1)).toEqual(["Down", "Enter"]);
  });

  it("maps a plain yes and rejects an unmappable reply", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    await look(rt, PERMISSION);
    const askId = ev.ask.mock.calls[0]![1] as string;
    expect(await rt.answerPrompt(askId, "what does that mean")).toBe(false);
    expect(await rt.answerPrompt(askId, "yes go ahead")).toBe(true);
    expect(tmux.keys.at(-1)).toEqual(["Enter"]);
  });

  it("matches an answer by the option's text", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    await look(rt, PERMISSION);
    const askId = ev.ask.mock.calls[0]![1] as string;
    expect(await rt.answerPrompt(askId, "no, and tell")).toBe(true);
    expect(tmux.keys.at(-1)).toEqual(["Down", "Down", "Enter"]);
  });

  it("will not answer a prompt that is no longer pending", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    await look(rt, PERMISSION);
    const askId = ev.ask.mock.calls[0]![1] as string;
    await look(rt, IDLE);
    expect(await rt.answerPrompt(askId, "1")).toBe(false);
    expect(await rt.answerPrompt("p/a#nope", "1")).toBe(false);
  });

  it("holds the queue and reports a usage limit once, then recovers", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    const limited = `${IDLE}\n Session limit reached · resets 3pm\n`;
    await look(rt, limited);
    await look(rt, limited);
    expect(ev.limit).toHaveBeenCalledTimes(1);
    expect(ev.limit).toHaveBeenCalledWith("p/a", { kind: "session", resetsAt: "3pm" });
    rt.enqueue({ from: "engineer", text: "go", msgId: "m1" });
    for (let i = 0; i < 4; i++) await look(rt, limited, 1000);
    expect(tmux.pasted).toEqual([]);
    await look(rt, IDLE, 3000);
    await look(rt, IDLE);
    expect(tmux.pasted).toHaveLength(1);
  });

  it("holds the queue while a blocking question is open", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    rt.beginAsk();
    rt.enqueue({ from: "engineer", text: "later", msgId: "m1" });
    await look(rt, IDLE);
    expect(tmux.pasted).toEqual([]);
    expect(ev.status).toHaveBeenLastCalledWith("p/a", "waiting_input", undefined);
    rt.endAsk();
    await look(rt, IDLE);
    expect(tmux.pasted).toHaveLength(1);
  });
});

describe("lifecycle", () => {
  it("reports when the pane has gone and stops looking", async () => {
    const { rt, ev } = make();
    await look(rt, IDLE);
    await rt.sample(null);
    expect(ev.gone).toHaveBeenCalledWith("p/a");
    expect(rt.isStopped).toBe(true);
    await rt.sample(IDLE);
    expect(ev.status).toHaveBeenCalledTimes(1);
  });

  it("kills its pane when stopped, and ignores screens afterwards", async () => {
    const { rt, ev } = make();
    await rt.stop();
    expect(tmux.killed).toEqual(["%1"]);
    await rt.sample(IDLE);
    expect(ev.status).not.toHaveBeenCalled();
  });

  it("survives a failure while handling a screen", async () => {
    const { rt, ev } = make();
    ev.status.mockImplementationOnce(() => {
      throw new Error("boom");
    });
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    await expect(rt.sample(IDLE)).resolves.toBeUndefined();
    expect(err).toHaveBeenCalled();
    err.mockRestore();
    await look(rt, BUSY);
    expect(ev.status).toHaveBeenCalled();
  });
});

describe("pacing, so idle agents cost almost nothing", () => {
  it("watches a starting agent closely", async () => {
    const { rt } = make();
    await rt.sample("booting...");
    expect(rt.nextInterval()).toBe(HOT_MS);
  });

  it("watches a working agent closely", async () => {
    const { rt } = make();
    await look(rt, IDLE);
    await look(rt, BUSY, 0);
    vi.advanceTimersByTime(COLD_AFTER_MS * 2);
    expect(rt.nextInterval()).toBe(HOT_MS);
  });

  it("slows down an agent whose screen has not changed, in two steps, and speeds up on a change", async () => {
    const { rt } = make();
    await look(rt, IDLE, 0);
    expect(rt.nextInterval()).toBe(HOT_MS);
    vi.advanceTimersByTime(WARM_AFTER_MS);
    expect(rt.nextInterval()).toBe(WARM_MS);
    vi.advanceTimersByTime(COLD_AFTER_MS - WARM_AFTER_MS);
    expect(rt.nextInterval()).toBe(COLD_MS);
    await rt.sample(IDLE + "\n new output\n");
    expect(rt.nextInterval()).toBe(HOT_MS);
  });

  it("schedules its next look after each sample, at the pace it needs", async () => {
    const { rt } = make();
    expect(rt.dueAt).toBe(0);
    await look(rt, IDLE, 0);
    expect(rt.dueAt - Date.now()).toBeGreaterThanOrEqual(HOT_MS / 2);
    expect(rt.dueAt - Date.now()).toBeLessThanOrEqual(HOT_MS * 1.5);
    vi.advanceTimersByTime(WARM_AFTER_MS + 100);
    await rt.sample(IDLE);
    expect(rt.dueAt - Date.now()).toBeGreaterThanOrEqual(WARM_MS / 2);
    expect(rt.dueAt % WARM_MS).toBe(0);
  });

  it("is woken at once by a message, a hold change, a question or an answer", async () => {
    const { rt } = make();
    const settle = async () => {
      await look(rt, IDLE, 0);
      vi.advanceTimersByTime(COLD_AFTER_MS + 1000);
      await rt.sample(IDLE);
      expect(rt.dueAt).toBeGreaterThan(Date.now());
    };
    await settle();
    rt.enqueue({ from: "engineer", text: "hi" });
    expect(rt.dueAt).toBe(0);
    await settle();
    rt.setHeld(true);
    expect(rt.dueAt).toBe(0);
    await settle();
    rt.beginAsk();
    expect(rt.dueAt).toBe(0);
    await settle();
    rt.endAsk();
    expect(rt.dueAt).toBe(0);
    await settle();
    rt.wake();
    expect(rt.dueAt).toBe(0);
  });

  it("keeps watching closely while a message is waiting for the agent to take it", async () => {
    const { rt } = make();
    await look(rt, IDLE);
    rt.enqueue({ from: "engineer", text: "x", msgId: "m1" });
    await look(rt, IDLE);
    vi.advanceTimersByTime(COLD_AFTER_MS * 2);
    expect(rt.nextInterval()).toBe(HOT_MS);
  });
});

describe("shared time grid", () => {
  it("snaps to a multiple of the interval, never earlier than half an interval away", () => {
    for (const interval of [HOT_MS, WARM_MS, COLD_MS]) {
      for (let now = 1_000_000; now < 1_000_000 + 5 * interval; now += 137) {
        const due = alignedDue(now, interval);
        expect(due % interval).toBe(0);
        expect(due - now).toBeGreaterThanOrEqual(interval / 2);
        expect(due - now).toBeLessThanOrEqual(interval * 1.5);
      }
    }
  });

  it("puts agents sampled at nearly the same time on at most two neighbouring instants", () => {
    // Rounding to a grid point can split a group at the boundary, but never by more than one step.
    const times = [1_000_000, 1_000_300, 1_000_600, 1_000_900, 1_001_200];
    const slots = [...new Set(times.map((t) => alignedDue(t, COLD_MS)))].sort((a, b) => a - b);
    expect(slots.length).toBeLessThanOrEqual(2);
    if (slots.length === 2) expect(slots[1]! - slots[0]!).toBe(COLD_MS);
  });

  it("keeps the three paces on one grid, so their rounds line up", () => {
    expect(WARM_MS % HOT_MS).toBe(0);
    expect(COLD_MS % WARM_MS).toBe(0);
    expect(HOT_MS % 250).toBe(0);
  });

  it("makes ten idle agents due at the same moment", async () => {
    const agents = Array.from({ length: 10 }, () => make().rt);
    for (let i = 0; i < agents.length; i++) {
      vi.advanceTimersByTime(37 * (i + 1));
      await agents[i]!.sample(IDLE);
    }
    vi.advanceTimersByTime(COLD_AFTER_MS + 500);
    for (const a of agents) await a.sample(IDLE);
    expect(new Set(agents.map((a) => a.dueAt)).size).toBeLessThanOrEqual(2);
  });
});
