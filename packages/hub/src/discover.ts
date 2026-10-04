/**
 * Works out everything about a Discord setup that can be worked out from the bot token alone, so setup asks for one
 * secret and nothing else: who owns the bot (that person is the owner of the hub), which servers it is in, and the
 * link to add it to one if it is in none.
 */
import { Client, GatewayIntentBits, type Team, type User } from "discord.js";
import { appIdFromToken, inviteUrl } from "./permissions.js";

export interface Guild {
  id: string;
  name: string;
}

export interface Discovery {
  botTag: string;
  appId: string;
  ownerId: string;
  ownerName: string;
  guilds: Guild[];
  invite: string;
}

/** The slice of a Discord client that discovery uses, so it can be tested without Discord. */
export interface DiscoveryClient {
  login(token: string): Promise<unknown>;
  destroy(): Promise<void> | void;
  user: { id: string; tag: string } | null;
  application: {
    fetch(): Promise<{
      id: string;
      owner: User | Team | { id?: string; ownerId?: string; username?: string; name?: string } | null;
    }>;
  } | null;
  guilds: { fetch(): Promise<Map<string, { id: string; name: string }>> };
  isReady?(): boolean;
  once?(event: "clientReady", fn: () => void): unknown;
}

export function makeClient(): DiscoveryClient {
  return new Client({ intents: [GatewayIntentBits.Guilds] }) as unknown as DiscoveryClient;
}

function ownerOf(app: { owner: unknown } | null): { id: string; name: string } | undefined {
  const o = app?.owner as
    | {
        id?: string;
        ownerId?: string;
        username?: string;
        name?: string;
        owner?: { id?: string; user?: { username?: string } };
      }
    | null
    | undefined;
  if (!o) return undefined;
  // A bot owned by a team has a Team here, whose owner is the person who made it.
  const id = o.ownerId ?? o.owner?.id ?? o.id;
  const name = o.owner?.user?.username ?? o.username ?? o.name ?? id;
  return id ? { id, name: name ?? id } : undefined;
}

export async function discover(token: string, client: DiscoveryClient = makeClient()): Promise<Discovery> {
  if (!appIdFromToken(token))
    throw new Error("That does not look like a Discord bot token. It starts with letters and has two dots in it.");
  try {
    await client.login(token);
  } catch (e) {
    throw new Error(
      `Discord rejected the token (${(e as Error).message}). Copy it again from the Bot page of your application.`,
      { cause: e },
    );
  }
  try {
    const app = await client.application?.fetch();
    const owner = ownerOf(app ?? null);
    if (!app || !owner) throw new Error("Could not find out who owns this bot.");
    const guilds = [...(await client.guilds.fetch()).values()].map((g) => ({ id: g.id, name: g.name }));
    return {
      botTag: client.user?.tag ?? "bot",
      appId: app.id,
      ownerId: owner.id,
      ownerName: owner.name,
      guilds,
      invite: inviteUrl(app.id),
    };
  } finally {
    await client.destroy();
  }
}

export interface WaitOptions {
  intervalMs?: number;
  timeoutMs?: number;
  onWait?: (secondsLeft: number) => void;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
}

/** After showing the invite link, wait for the bot to appear in a server, so setup carries on by itself. */
export async function waitForGuild(
  token: string,
  makeOne: () => DiscoveryClient,
  o: WaitOptions = {},
): Promise<Discovery> {
  const interval = o.intervalMs ?? 3000;
  const timeout = o.timeoutMs ?? 10 * 60_000;
  const sleep = o.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  const now = o.now ?? Date.now;
  const end = now() + timeout;
  for (;;) {
    const d = await discover(token, makeOne());
    if (d.guilds.length) return d;
    if (now() >= end)
      throw new Error("The bot was not added to a server in time. Run setup again once you have used the invite link.");
    o.onWait?.(Math.round((end - now()) / 1000));
    await sleep(interval);
  }
}
