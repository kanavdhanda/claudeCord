import { describe, expect, it, vi } from "vitest";
import type { HubConfig } from "../src/config.js";
import type { DiscoveryClient } from "../src/discover.js";
import { runSetup, type SetupIO } from "../src/setup.js";

const APP_ID = "123456789012345678";
const OWNER = "223456789012345678";
const TOKEN = `${Buffer.from(APP_ID).toString("base64")}.${"a".repeat(6)}.${"b".repeat(27)}`;

function fakeClient(guildsNow: () => { id: string; name: string }[]): DiscoveryClient {
  return {
    login: async () => {},
    destroy: () => {},
    user: { id: APP_ID, tag: "claudecord#0001" },
    application: { fetch: async () => ({ id: APP_ID, owner: { id: OWNER, username: "kanav" } as never }) },
    guilds: { fetch: async () => new Map(guildsNow().map((g) => [g.id, g])) },
  };
}

function harness(guilds: () => { id: string; name: string }[], answers: Record<string, string> = {}) {
  const log: string[] = [];
  const saved: HubConfig[] = [];
  const asked: string[] = [];
  const opened: string[] = [];
  const io: SetupIO = {
    ask: async (q, o) => {
      asked.push(q);
      if (o?.hidden) return answers.token ?? TOKEN;
      const key = Object.keys(answers).find((k) => q.includes(k));
      return key ? answers[key]! : (o?.default ?? "");
    },
    log: (l) => void log.push(l),
    makeClient: () => fakeClient(guilds),
    save: (c) => void saved.push(c),
    open: (u) => void opened.push(u),
  };
  return { io, log, saved, asked, opened };
}

const defaults = { dbPath: "/tmp/hub.db", intervalMs: 1, waitMs: 200 };

describe("setup", () => {
  it("asks only for the token and where the hub will be, when the bot is already in one server", async () => {
    const h = harness(() => [{ id: "323456789012345678", name: "My Server" }]);
    const cfg = await runSetup(h.io, defaults);
    expect(h.asked).toEqual(["Bot token", "\nAddress devices and the dashboard will use to reach this hub"]);
    expect(cfg).toMatchObject({
      discordToken: TOKEN,
      guildId: "323456789012345678",
      ownerId: OWNER,
      publicUrl: "http://localhost:8787",
      port: 8787,
      dbPath: "/tmp/hub.db",
    });
    expect(h.saved).toEqual([cfg]);
    expect(h.log.join("\n")).toContain("Owner: kanav");
    expect(h.opened).toEqual([]);
  });

  it("shows the invite link and waits when the bot is in no server, then carries on", async () => {
    let polls = 0;
    const h = harness(() => (++polls > 3 ? [{ id: "323456789012345678", name: "Fresh Server" }] : []));
    const cfg = await runSetup(h.io, defaults);
    const text = h.log.join("\n");
    expect(text).toContain("not in any server yet");
    expect(text).toContain(`client_id=${APP_ID}`);
    expect(text).toContain("309774625872");
    expect(text).toContain("not Administrator");
    expect(text).toContain("Waiting for the bot to join");
    expect(text).toContain("joined Fresh Server");
    expect(h.opened).toHaveLength(1);
    expect(h.opened[0]).toContain("discord.com/oauth2/authorize");
    expect(cfg.guildId).toBe("323456789012345678");
  });

  it("still works when opening the browser fails", async () => {
    const h = harness(() => []);
    h.io.open = () => {
      throw new Error("no browser");
    };
    await expect(runSetup(h.io, { ...defaults, waitMs: 20 })).rejects.toThrow(/not added to a server in time/);
    expect(h.log.join("\n")).toContain("client_id=");
  });

  it("lets you choose when the bot is in several servers", async () => {
    const guilds = [
      { id: "323456789012345678", name: "First" },
      { id: "423456789012345678", name: "Second" },
    ];
    const h = harness(() => guilds, { "Which one": "2" });
    const cfg = await runSetup(h.io, defaults);
    expect(cfg.guildId).toBe("423456789012345678");
    expect(h.log.join("\n")).toContain("2. Second");
  });

  it("falls back to the first server on a nonsense choice", async () => {
    const guilds = [
      { id: "323456789012345678", name: "First" },
      { id: "423456789012345678", name: "Second" },
    ];
    const h = harness(() => guilds, { "Which one": "banana" });
    expect((await runSetup(h.io, defaults)).guildId).toBe("323456789012345678");
  });

  it("uses an owner given on the command line instead of the bot's owner", async () => {
    const h = harness(() => [{ id: "323456789012345678", name: "S" }]);
    const cfg = await runSetup(h.io, { ...defaults, owner: "523456789012345678" });
    expect(cfg.ownerId).toBe("523456789012345678");
  });

  it("accepts an https public address and a custom port", async () => {
    const h = harness(() => [{ id: "323456789012345678", name: "S" }], { Address: "https://hub.example.com" });
    const cfg = await runSetup(h.io, { ...defaults, port: 9000 });
    expect(cfg.publicUrl).toBe("https://hub.example.com");
    expect(cfg.port).toBe(9000);
  });

  it("refuses an address that is not https, and saves nothing", async () => {
    const h = harness(() => [{ id: "323456789012345678", name: "S" }], { Address: "http://hub.example.com" });
    await expect(runSetup(h.io, defaults)).rejects.toThrow(/https/);
    expect(h.saved).toEqual([]);
  });

  it("never prints the token", async () => {
    const h = harness(() => [{ id: "323456789012345678", name: "S" }]);
    await runSetup(h.io, defaults);
    expect(h.log.join("\n")).not.toContain(TOKEN);
  });

  it("stops with Discord's reason when the token is wrong, and saves nothing", async () => {
    const h = harness(() => []);
    h.io.makeClient = () => ({
      ...fakeClient(() => []),
      login: async () => Promise.reject(new Error("An invalid token was provided.")),
    });
    await expect(runSetup(h.io, defaults)).rejects.toThrow(/Discord rejected the token/);
    expect(h.saved).toEqual([]);
  });

  it("stops early on text that is not a token", async () => {
    const h = harness(() => [], { token: "hello" });
    const make = vi.spyOn(h.io, "makeClient");
    await expect(runSetup(h.io, defaults)).rejects.toThrow(/does not look like a Discord bot token/);
    expect(make).toHaveBeenCalledTimes(1);
  });
});
