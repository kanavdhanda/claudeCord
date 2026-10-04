import { describe, expect, it } from "vitest";
import { agy } from "../src/adapters/agy.js";
import { codex } from "../src/adapters/codex.js";
import { getAdapter } from "../src/adapters/index.js";

const spec = (o: Record<string, unknown> = {}) => ({
  agentId: "p/a",
  name: "a",
  project: "p",
  adapter: "agy" as const,
  ...o,
});

const MENU = `
 Do you trust the files in this folder?
 ❯ 1. Yes, proceed
   2. No, exit
`;

describe("agy adapter", () => {
  it("builds its command from the policy and model", () => {
    expect(agy.argv({ spec: spec(), policy: "ask", rules: "" })).toEqual(["agy"]);
    expect(agy.argv({ spec: spec({ model: "gemini-3" }), policy: "autonomous", rules: "" })).toEqual([
      "agy",
      "--model",
      "gemini-3",
      "--dangerously-skip-permissions",
    ]);
    expect(agy.argv({ spec: spec(), policy: "plan", rules: "" })).toEqual(["agy", "--mode", "plan"]);
  });

  it("gets its rules as a first message because it has no system prompt flag", () => {
    expect(agy.rulesVia).toBe("first-message");
  });

  it("reads busy, ready, prompt and limit from the screen", () => {
    expect(agy.detect("\n ready for input\n")).toMatchObject({ ready: true, busy: false });
    expect(agy.detect("\n working on it (esc to cancel)\n")).toMatchObject({ ready: false, busy: true });
    const withMenu = agy.detect(MENU);
    expect(withMenu.prompt?.options[0]).toBe("Yes, proceed");
    expect(withMenu.ready).toBe(false);
    expect(agy.detect("\n You've hit your weekly limit · resets Oct 9\n").limit?.kind).toBe("weekly");
    expect(agy.detect("\n rate limit exceeded, try again soon\n").limit?.kind).toBe("rate");
  });

  it("answers only the folder trust dialog by itself", () => {
    const p = agy.detect(MENU).prompt!;
    expect(agy.startupChoice(p)).toBe(0);
    expect(agy.startupChoice({ ...p, question: "Run this command?", options: ["Run", "Skip"] })).toBeUndefined();
    expect(agy.selectKeys(p, 1)).toEqual(["Down", "Enter"]);
    expect(agy.otherKeys(p)).toBeUndefined();
  });
});

describe("codex adapter", () => {
  it("maps each policy to a sandbox and approval setting, and never uses the alternate screen", () => {
    const base = { spec: spec({ adapter: "codex", model: "gpt-5" }), rules: "" };
    expect(codex.argv({ ...base, policy: "autonomous" })).toEqual([
      "codex",
      "--no-alt-screen",
      "-m",
      "gpt-5",
      "-s",
      "workspace-write",
      "-a",
      "never",
    ]);
    expect(codex.argv({ ...base, policy: "plan" })).toEqual([
      "codex",
      "--no-alt-screen",
      "-m",
      "gpt-5",
      "-s",
      "read-only",
      "-a",
      "on-request",
    ]);
    expect(codex.argv({ ...base, policy: "ask" })).toEqual([
      "codex",
      "--no-alt-screen",
      "-m",
      "gpt-5",
      "-s",
      "workspace-write",
      "-a",
      "on-request",
    ]);
  });

  it("reads busy, ready, prompt and limit from the screen", () => {
    expect(codex.detect("\n > \n").ready).toBe(true);
    expect(codex.detect("\n thinking (esc to interrupt)\n").busy).toBe(true);
    expect(codex.detect(MENU).prompt?.question).toContain("trust");
    expect(codex.detect("\n Session limit reached · resets 4pm\n").limit).toMatchObject({
      kind: "session",
      resetsAt: "4pm",
    });
  });

  it("answers the trust dialog and nothing else at startup", () => {
    const p = codex.detect(MENU).prompt!;
    expect(codex.startupChoice(p)).toBe(0);
    expect(codex.startupChoice({ ...p, question: "Apply patch?", options: ["Yes", "No"] })).toBeUndefined();
  });
});

describe("adapter registry", () => {
  it("returns the adapter for each id", () => {
    expect(getAdapter("agy")).toBe(agy);
    expect(getAdapter("codex")).toBe(codex);
    expect(getAdapter("claude").binary).toBe("claude");
  });
});
