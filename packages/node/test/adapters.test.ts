import { describe, expect, it } from "vitest";
import { claude } from "../src/adapters/claude.js";
import { codex } from "../src/adapters/codex.js";
import { detectLimit, menuKeys, parseMenu } from "../src/adapters/types.js";

const PERMISSION = `
 Bash command

   npm install express

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for npm commands
   3. No, and tell Claude what to do differently (esc)
`;

const TRUST = `
 Do you trust the files in this folder?

 ❯ 1. Yes, proceed
   2. No, exit

 Enter to confirm · Esc to cancel
`;

const IDLE = `
 │ > Try "fix the tests"
 ? for shortcuts
`;

const BUSY = `
 ✽ Thinking… (12s · esc to interrupt)
 ? for shortcuts
`;

describe("menu parsing", () => {
  it("parses a permission prompt and cursor", () => {
    const p = parseMenu(PERMISSION)!;
    expect(p.question).toBe("Do you want to proceed?");
    expect(p.options).toHaveLength(3);
    expect(p.cursor).toBe(0);
  });

  it("ignores numbered lists with no cursor marker", () => {
    expect(parseMenu("Plan:\n1. do a\n2. do b\n3. do c\n")).toBeUndefined();
  });

  it("builds navigation keys relative to the cursor", () => {
    const p = parseMenu(PERMISSION)!;
    expect(menuKeys(p, 2)).toEqual(["Down", "Down", "Enter"]);
    expect(menuKeys({ ...p, cursor: 2 }, 0)).toEqual(["Up", "Up", "Enter"]);
  });
});

describe("claude adapter", () => {
  it("detects prompt, idle and busy states", () => {
    expect(claude.detect(PERMISSION).prompt?.options[0]).toBe("Yes");
    expect(claude.detect(IDLE)).toMatchObject({ ready: true, busy: false });
    expect(claude.detect(BUSY)).toMatchObject({ ready: false, busy: true });
  });

  it("auto-answers the folder trust dialog with yes", () => {
    const p = claude.detect(TRUST).prompt!;
    expect(claude.startupChoice(p)).toBe(0);
  });

  it("does not auto-answer ordinary permission prompts", () => {
    expect(claude.startupChoice(claude.detect(PERMISSION).prompt!)).toBeUndefined();
  });

  it("builds argv per policy", () => {
    const base = {
      spec: { agentId: "p/a", name: "a", project: "p", adapter: "claude" as const, model: "sonnet" },
      rules: "R",
    };
    expect(claude.argv({ ...base, policy: "autonomous" })).toContain("--dangerously-skip-permissions");
    expect(claude.argv({ ...base, policy: "plan" })).toEqual(expect.arrayContaining(["--permission-mode", "plan"]));
    expect(claude.argv({ ...base, policy: "ask" })).toEqual(
      expect.arrayContaining(["--model", "sonnet", "--append-system-prompt", "R"]),
    );
  });
});

describe("limits", () => {
  it("detects session and weekly limits with reset time", () => {
    expect(detectLimit("\nClaude usage limit reached. Session limit reached · resets 3pm\n")).toMatchObject({
      kind: "session",
      resetsAt: "3pm",
    });
    expect(detectLimit("You've hit your weekly limit · resets Oct 9, 9am")).toMatchObject({ kind: "weekly" });
  });

  it("ignores limit wording far up in the scrollback", () => {
    const old = "session limit reached\n" + "line\n".repeat(30);
    expect(detectLimit(old)).toBeUndefined();
  });
});

describe("codex adapter", () => {
  it("maps policy to sandbox and approval flags", () => {
    const spec = { agentId: "p/a", name: "a", project: "p", adapter: "codex" as const };
    expect(codex.argv({ spec, rules: "", policy: "autonomous" })).toEqual(expect.arrayContaining(["-a", "never"]));
    expect(codex.argv({ spec, rules: "", policy: "plan" })).toEqual(expect.arrayContaining(["-s", "read-only"]));
  });
});
