import { describe, it, expect } from "vitest";
import { readEnvValue, upsertEnv } from "./env-file.js";

describe("env file", () => {
  it("updates existing keys and appends new ones, keeping comments", () => {
    const before = "# my config\nDISCORD_BOT_TOKEN=old\nSHOW_COST=true\n";
    const after = upsertEnv(before, { DISCORD_BOT_TOKEN: "new$1", ACCESS_MODE: "anyone" });
    expect(after).toBe("# my config\nDISCORD_BOT_TOKEN=new$1\nSHOW_COST=true\nACCESS_MODE=anyone\n");
  });
  it("handles empty files and a missing trailing newline", () => {
    expect(upsertEnv("", { A: "1" })).toBe("A=1\n");
    expect(upsertEnv("A=1", { B: "2" })).toBe("A=1\nB=2\n");
  });
  it("does not match keys by prefix", () => {
    expect(readEnvValue("XDISCORD_BOT_TOKEN=x\nDISCORD_BOT_TOKEN=y", "DISCORD_BOT_TOKEN")).toBe("y");
    expect(readEnvValue("A=1", "B")).toBeUndefined();
  });
  it("refuses values that would inject lines", () => {
    expect(() => upsertEnv("", { A: "x\nEVIL=1" })).toThrow();
  });
});
