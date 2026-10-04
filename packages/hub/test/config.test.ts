import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { chmodSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { HubConfigSchema, loadConfig, maskToken, saveConfig } from "../src/config.js";

const good = {
  discordToken: "x".repeat(70),
  guildId: "123456789012345678",
  ownerId: "223456789012345678",
  publicUrl: "https://hub.example.com",
  categoryName: "claudecord",
  port: 8787,
  dbPath: "/tmp/hub.db",
};

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "cc-cfg-"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const posix = process.platform !== "win32";

describe("hub config file", () => {
  it("saves privately and loads back", () => {
    const p = join(dir, "sub", "hub.json");
    saveConfig(p, good);
    expect(loadConfig(p)).toEqual(good);
    if (posix) {
      expect(statSync(p).mode & 0o077).toBe(0);
      expect(statSync(join(dir, "sub")).mode & 0o077).toBe(0);
    }
  });

  it.skipIf(!posix)("refuses a config that other users can read, and says how to fix it", () => {
    const p = join(dir, "hub.json");
    saveConfig(p, good);
    chmodSync(p, 0o644);
    expect(() => loadConfig(p)).toThrow(/readable by other users.*chmod 600/s);
    chmodSync(p, 0o640);
    expect(() => loadConfig(p)).toThrow(/readable by other users/);
    chmodSync(p, 0o600);
    expect(() => loadConfig(p)).not.toThrow();
  });

  it("explains a missing file and invalid JSON", () => {
    expect(() => loadConfig(join(dir, "none.json"))).toThrow(/claudecord-hub setup/);
    const p = join(dir, "bad.json");
    writeFileSync(p, "{ nope", { mode: 0o600 });
    expect(() => loadConfig(p)).toThrow(/not valid JSON/);
  });

  it("lists every problem in a bad config", () => {
    const p = join(dir, "hub.json");
    writeFileSync(p, JSON.stringify({ ...good, guildId: "abc", publicUrl: "http://example.com", discordToken: "short" }), { mode: 0o600 });
    try {
      loadConfig(p);
      throw new Error("should have thrown");
    } catch (e) {
      const m = (e as Error).message;
      expect(m).toContain("guildId");
      expect(m).toContain("publicUrl");
      expect(m).toContain("discordToken");
    }
  });

  it("does not read the environment", () => {
    process.env.DISCORD_TOKEN = "z".repeat(70);
    try {
      expect(() => loadConfig(join(dir, "none.json"))).toThrow(/No hub config/);
    } finally {
      delete process.env.DISCORD_TOKEN;
    }
  });

  it("never writes a token anywhere but the config file", () => {
    const p = join(dir, "hub.json");
    saveConfig(p, good);
    expect(readFileSync(p, "utf8")).toContain(good.discordToken);
    expect(maskToken(good.discordToken)).not.toContain("x".repeat(10));
  });
});

describe("hub config rules", () => {
  const parse = (o: object) => HubConfigSchema.safeParse({ ...good, ...o });

  it.each(["https://hub.example.com", "https://hub.example.com:8443", "http://localhost:8787", "http://127.0.0.1:8787"])("accepts public url %s", (publicUrl) => {
    expect(parse({ publicUrl }).success).toBe(true);
  });

  it.each(["http://hub.example.com", "ftp://hub.example.com", "hub.example.com", "http://192.168.1.5:8787"])("rejects public url %s", (publicUrl) => {
    expect(parse({ publicUrl }).success).toBe(false);
  });

  it.each(["", "12", "abc", "1".repeat(21), "12345678901234567x"])("rejects Discord id %j", (id) => {
    expect(parse({ guildId: id }).success).toBe(false);
    expect(parse({ ownerId: id }).success).toBe(false);
  });

  it("rejects a bad port and an avatar template that is not a URL", () => {
    expect(parse({ port: 70000 }).success).toBe(false);
    expect(parse({ port: -1 }).success).toBe(false);
    expect(parse({ avatarTemplate: "not a url" }).success).toBe(false);
    expect(parse({ avatarTemplate: "https://a.example/{name}.png" }).success).toBe(true);
  });

  it("masks a token for display", () => {
    expect(maskToken("abcdefghijklmnop")).toBe("abcd...mnop");
    expect(maskToken("short")).toBe("****");
  });
});
