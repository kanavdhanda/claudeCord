import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const tmux = vi.hoisted(() => ({
  screen: "",
  pasted: [] as string[],
  keys: [] as string[][],
  alive: true,
}));

vi.mock("../src/tmux.js", () => ({
  paneAlive: async () => tmux.alive,
  capture: async () => tmux.screen,
  pasteAndSubmit: async (_p: string, text: string) => void tmux.pasted.push(text),
  sendKeys: async (_p: string, keys: string[]) => void tmux.keys.push(keys),
  killPane: async () => {},
}));

import { claude } from "../src/adapters/claude.js";
import { AgentRuntime, type AgentEvents } from "../src/agent.js";

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

function make() {
  const ev = {
    status: vi.fn(),
    ask: vi.fn(),
    limit: vi.fn(),
    gone: vi.fn(),
    accepted: vi.fn(),
  } satisfies AgentEvents;
  const spec = { agentId: "p/a", name: "a", project: "p", adapter: "claude" as const };
  const rt = new AgentRuntime(spec, "%1", "/tmp/p", claude, ev);
  rt.start();
  return { rt, ev };
}

const tick = (ms = 900) => vi.advanceTimersByTimeAsync(ms);

beforeEach(() => {
  vi.useFakeTimers();
  tmux.screen = IDLE;
  tmux.pasted = [];
  tmux.keys = [];
  tmux.alive = true;
});
afterEach(() => vi.useRealTimers());

describe("agent runtime", () => {
  it("delivers queued messages when idle and confirms acceptance once the agent is busy", async () => {
    const { rt, ev } = make();
    await tick(); // becomes ready
    rt.enqueue({ from: "engineer", text: "build the migration", msgId: "m1" });
    await tick();
    expect(tmux.pasted).toEqual(["[engineer] build the migration"]);
    expect(ev.accepted).not.toHaveBeenCalled();
    tmux.screen = BUSY;
    await tick();
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
    await tick();
    expect(ev.accepted).toHaveBeenCalledTimes(1);
  });

  it("recognises acceptance from the echoed text when the turn finishes between samples", async () => {
    const { rt, ev } = make();
    await tick();
    rt.enqueue({ from: "engineer", text: "rename the helper function", msgId: "m1" });
    await tick();
    tmux.screen = `${IDLE}\n > [engineer] rename the helper function\n`;
    await tick();
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
  });

  it("does not claim acceptance while the pane just sits idle", async () => {
    const { rt, ev } = make();
    await tick();
    rt.enqueue({ from: "engineer", text: "do something", msgId: "m1" });
    await tick();
    await tick(5000);
    expect(ev.accepted).not.toHaveBeenCalled();
  });

  it("treats the agent speaking as acceptance", async () => {
    const { rt, ev } = make();
    await tick();
    rt.enqueue({ from: "engineer", text: "do something", msgId: "m1" });
    await tick();
    rt.noteActivity();
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1"]);
  });

  it("batches several waiting messages into one injection and accepts them together", async () => {
    const { rt, ev } = make();
    await tick();
    rt.setHeld(true);
    rt.enqueue({ from: "engineer", text: "first", msgId: "m1" });
    rt.enqueue({ from: "otter", text: "second", msgId: "m2" });
    await tick(3000);
    expect(tmux.pasted).toEqual([]);
    rt.setHeld(false);
    await tick();
    expect(tmux.pasted).toEqual(["[engineer] first\n\n[otter] second"]);
    tmux.screen = BUSY;
    await tick();
    expect(ev.accepted).toHaveBeenCalledWith("p/a", ["m1", "m2"]);
  });

  it("does not inject while the agent is busy", async () => {
    const { rt } = make();
    await tick();
    tmux.screen = BUSY;
    rt.enqueue({ from: "engineer", text: "later", msgId: "m1" });
    await tick(4000);
    expect(tmux.pasted).toEqual([]);
    tmux.screen = IDLE;
    await tick();
    expect(tmux.pasted).toHaveLength(1);
  });

  it("answers the folder trust dialog by itself at startup", async () => {
    make();
    tmux.screen = TRUST;
    await tick();
    expect(tmux.keys[0]).toEqual(["Enter"]);
  });

  it("relays a permission prompt once and maps the human's reply onto the menu", async () => {
    const { rt, ev } = make();
    await tick();
    tmux.screen = PERMISSION;
    await tick();
    await tick();
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
    await tick();
    tmux.screen = PERMISSION;
    await tick();
    const askId = ev.ask.mock.calls[0]![1] as string;
    expect(await rt.answerPrompt(askId, "what does that mean")).toBe(false);
    expect(await rt.answerPrompt(askId, "yes go ahead")).toBe(true);
    expect(tmux.keys.at(-1)).toEqual(["Enter"]);
  });

  it("holds the queue and reports a usage limit once, then recovers", async () => {
    const { rt, ev } = make();
    await tick();
    tmux.screen = `${IDLE}\n Session limit reached · resets 3pm\n`;
    await tick();
    await tick();
    expect(ev.limit).toHaveBeenCalledTimes(1);
    expect(ev.limit).toHaveBeenCalledWith("p/a", { kind: "session", resetsAt: "3pm" });
    rt.enqueue({ from: "engineer", text: "go", msgId: "m1" });
    await tick(4000);
    expect(tmux.pasted).toEqual([]);
    tmux.screen = IDLE;
    await tick(3000);
    expect(tmux.pasted).toHaveLength(1);
  });

  it("reports when the pane disappears", async () => {
    const { ev } = make();
    await tick();
    tmux.alive = false;
    await tick();
    expect(ev.gone).toHaveBeenCalledWith("p/a");
  });
});
