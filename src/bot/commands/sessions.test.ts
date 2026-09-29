import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import {
  findSessionDir,
  getLastAssistantMessage,
  getLastAssistantMessageFull,
} from "./sessions.js";

// Mock database imports used by the module
vi.mock("../../db/database.js", () => ({
  getProject: vi.fn(),
  getSession: vi.fn(),
}));

// ─── findSessionDir ───

describe("findSessionDir", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns null when ~/.claude/projects does not exist", () => {
    vi.spyOn(os, "homedir").mockReturnValue("/fake/home");
    vi.spyOn(fs, "existsSync").mockReturnValue(false);
    expect(findSessionDir("/my/project")).toBeNull();
  });

  it("returns encoded path when simple conversion matches", () => {
    vi.spyOn(os, "homedir").mockReturnValue("/fake/home");
    vi.spyOn(fs, "existsSync").mockImplementation((p) => {
      const ps = String(p);
      if (ps === "/fake/home/.claude/projects") return true;
      if (ps === "/fake/home/.claude/projects/-my-project") return true;
      return false;
    });
    expect(findSessionDir("/my/project")).toBe(
      "/fake/home/.claude/projects/-my-project",
    );
  });

  it("returns null when no matching directory found", () => {
    vi.spyOn(os, "homedir").mockReturnValue("/fake/home");
    vi.spyOn(fs, "existsSync").mockImplementation((p) => {
      const ps = String(p);
      return ps === "/fake/home/.claude/projects";
    });
    vi.spyOn(fs, "readdirSync").mockReturnValue([] as unknown as ReturnType<typeof fs.readdirSync>);
    expect(findSessionDir("/unknown/project")).toBeNull();
  });
});

// ─── getLastAssistantMessage ───

describe("getLastAssistantMessage", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "sessions-test-"));
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  function writeJsonl(filename: string, lines: unknown[]): string {
    const filePath = path.join(tmpDir, filename);
    fs.writeFileSync(filePath, lines.map((l) => JSON.stringify(l)).join("\n"));
    return filePath;
  }

  it("returns last assistant text (last line of last message)", async () => {
    const file = writeJsonl("test.jsonl", [
      { type: "assistant", message: { content: [{ type: "text", text: "First" }] } },
      { type: "assistant", message: { content: [{ type: "text", text: "Second" }] } },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("Second");
  });

  it("handles string content", async () => {
    const file = writeJsonl("test.jsonl", [
      { type: "assistant", message: { content: "Simple string" } },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("Simple string");
  });

  it("returns last line of multi-line text", async () => {
    const file = writeJsonl("test.jsonl", [
      { type: "assistant", message: { content: [{ type: "text", text: "Line 1\nLine 2\nLine 3" }] } },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("Line 3");
  });

  it("returns '(no message)' when no assistant messages", async () => {
    const file = writeJsonl("test.jsonl", [
      { type: "user", message: { content: "hello" } },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("(no message)");
  });

  it("skips malformed JSON lines", async () => {
    const filePath = path.join(tmpDir, "test.jsonl");
    const lines = [
      "not json",
      JSON.stringify({ type: "assistant", message: { content: [{ type: "text", text: "Valid" }] } }),
      "{broken",
    ];
    fs.writeFileSync(filePath, lines.join("\n"));
    expect(await getLastAssistantMessage(filePath)).toBe("Valid");
  });

  it("skips assistant messages with empty/whitespace text", async () => {
    const file = writeJsonl("test.jsonl", [
      { type: "assistant", message: { content: [{ type: "text", text: "Good" }] } },
      { type: "assistant", message: { content: [{ type: "text", text: "   \n  " }] } },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("Good");
  });

  it("handles multiple text blocks in content array", async () => {
    const file = writeJsonl("test.jsonl", [
      {
        type: "assistant",
        message: {
          content: [
            { type: "text", text: "Part A " },
            { type: "text", text: "Part B" },
          ],
        },
      },
    ]);
    expect(await getLastAssistantMessage(file)).toBe("Part A Part B");
  });
});

// ─── getLastAssistantMessageFull ───

describe("getLastAssistantMessageFull", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "sessions-test-"));
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  it("returns full text instead of just last line", async () => {
    const filePath = path.join(tmpDir, "test.jsonl");
    const line = JSON.stringify({
      type: "assistant",
      message: { content: [{ type: "text", text: "Line 1\nLine 2\nLine 3" }] },
    });
    fs.writeFileSync(filePath, line);
    const result = await getLastAssistantMessageFull(filePath);
    expect(result).toContain("Line 1");
    expect(result).toContain("Line 2");
    expect(result).toContain("Line 3");
  });

  it("returns '(no message)' when no assistant messages", async () => {
    const filePath = path.join(tmpDir, "test.jsonl");
    fs.writeFileSync(filePath, JSON.stringify({ type: "user", message: { content: "hi" } }));
    expect(await getLastAssistantMessageFull(filePath)).toBe("(no message)");
  });
});
