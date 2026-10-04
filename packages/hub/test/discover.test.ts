import { describe, expect, it, vi } from "vitest";
import { discover, waitForGuild, type DiscoveryClient } from "../src/discover.js";

const APP_ID = "123456789012345678";
const TOKEN = `${Buffer.from(APP_ID).toString("base64")}.${"a".repeat(6)}.${"b".repeat(27)}`;

function client(
  over: Partial<{ owner: unknown; guilds: { id: string; name: string }[]; loginError: string; appError: boolean }> = {},
) {
  const destroyed = vi.fn();
  const c: DiscoveryClient = {
    login: async () => {
      if (over.loginError) throw new Error(over.loginError);
    },
    destroy: destroyed,
    user: { id: APP_ID, tag: "claudecord#0001" },
    application: {
      fetch: async () => {
        if (over.appError) throw new Error("no access");
        return {
          id: APP_ID,
          owner: ("owner" in over ? over.owner : { id: "223456789012345678", username: "kanav" }) as never,
        };
      },
    },
    guilds: { fetch: async () => new Map((over.guilds ?? []).map((g) => [g.id, g])) },
  };
  return { c, destroyed };
}

describe("discover", () => {
  it("finds the owner, the servers and the invite link from the token alone", async () => {
    const { c, destroyed } = client({ guilds: [{ id: "323456789012345678", name: "My Server" }] });
    const d = await discover(TOKEN, c);
    expect(d.botTag).toBe("claudecord#0001");
    expect(d.appId).toBe(APP_ID);
    expect(d.ownerId).toBe("223456789012345678");
    expect(d.ownerName).toBe("kanav");
    expect(d.guilds).toEqual([{ id: "323456789012345678", name: "My Server" }]);
    expect(d.invite).toContain(`client_id=${APP_ID}`);
    expect(destroyed).toHaveBeenCalled();
  });

  it("uses the person who made the bot when it belongs to a team", async () => {
    const team = {
      id: "999999999999999999",
      name: "My Team",
      ownerId: "223456789012345678",
      owner: { user: { username: "kanav" } },
    };
    const { c } = client({ owner: team });
    const d = await discover(TOKEN, c);
    expect(d.ownerId).toBe("223456789012345678");
    expect(d.ownerName).toBe("kanav");
  });

  it("returns no servers when the bot has not been added to one yet", async () => {
    const { c } = client();
    expect((await discover(TOKEN, c)).guilds).toEqual([]);
  });

  it("rejects something that is not a token without contacting Discord", async () => {
    const login = vi.fn();
    const { c } = client();
    c.login = login;
    await expect(discover("not-a-token", c)).rejects.toThrow(/does not look like a Discord bot token/);
    expect(login).not.toHaveBeenCalled();
  });

  it("explains a token Discord refuses", async () => {
    const { c } = client({ loginError: "An invalid token was provided." });
    await expect(discover(TOKEN, c)).rejects.toThrow(/Discord rejected the token.*invalid token/s);
  });

  it("says so when the owner cannot be found, and still lets go of the connection", async () => {
    const { c, destroyed } = client({ owner: null });
    await expect(discover(TOKEN, c)).rejects.toThrow(/who owns this bot/);
    expect(destroyed).toHaveBeenCalled();
    const { c: c2, destroyed: d2 } = client({ appError: true });
    await expect(discover(TOKEN, c2)).rejects.toThrow();
    expect(d2).toHaveBeenCalled();
  });
});

describe("waitForGuild", () => {
  it("keeps checking until the bot appears in a server", async () => {
    let calls = 0;
    const make = () => client({ guilds: ++calls < 4 ? [] : [{ id: "323456789012345678", name: "Joined" }] }).c;
    const waits: number[] = [];
    const d = await waitForGuild(TOKEN, make, {
      sleep: async () => {},
      onWait: (s) => waits.push(s),
      timeoutMs: 60_000,
    });
    expect(calls).toBe(4);
    expect(d.guilds[0]!.name).toBe("Joined");
    expect(waits).toHaveLength(3);
  });

  it("gives up with a clear message after the timeout", async () => {
    let t = 0;
    await expect(
      waitForGuild(TOKEN, () => client().c, {
        sleep: async () => void (t += 5000),
        now: () => t,
        timeoutMs: 20_000,
        intervalMs: 5000,
      }),
    ).rejects.toThrow(/not added to a server in time/);
  });
});
