import { describe, it, expect } from "vitest";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { latestSessionId, transcriptDirFor } from "./session-files.js";

describe("session files", () => {
  it("maps a project path to Claude Code's transcript folder", () => {
    expect(transcriptDirFor("/Users/kd/Developer/claudeCord", "/h")).toBe("/h/.claude/projects/-Users-kd-Developer-claudeCord");
  });
  it("finds the newest session", () => {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), "sf-"));
    const proj = "/work/app";
    const dir = transcriptDirFor(proj, home);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "old.jsonl"), "{}");
    fs.utimesSync(path.join(dir, "old.jsonl"), new Date(1000), new Date(1000));
    fs.writeFileSync(path.join(dir, "new.jsonl"), "{}");
    fs.writeFileSync(path.join(dir, "notes.txt"), "x");
    expect(latestSessionId(proj, home)).toBe("new");
  });
  it("returns null when there is nothing", () => {
    expect(latestSessionId("/nope", fs.mkdtempSync(path.join(os.tmpdir(), "sf-")))).toBeNull();
  });
});
