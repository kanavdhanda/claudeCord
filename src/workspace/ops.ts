import { ChannelType, Client, GatewayIntentBits, type Guild, type TextChannel } from "discord.js";
import { announce, ensureRole, scanChannelForPeers } from "../agents/persona.js";
import { agentKeyFor, safePersonaName } from "../agents/identity.js";
import { getAgentsInChannel, getDirectoryForChannel, getProject, registerAgent } from "../db/database.js";

/** Log in briefly (no message intents needed), run something, and disconnect. */
export async function withClient<T>(botToken: string, fn: (client: Client) => Promise<T>): Promise<T> {
  const client = new Client({ intents: [GatewayIntentBits.Guilds] });
  const ready = new Promise<void>((resolve) => client.once("clientReady", () => resolve()));
  await client.login(botToken).catch((e) => {
    throw new Error(`Discord rejected the bot token (${e instanceof Error ? e.message : e}).`);
  });
  await ready;
  try {
    return await fn(client);
  } finally {
    await client.destroy();
  }
}

export function pickGuild(client: Client, wanted?: string): Guild {
  const guilds = [...client.guilds.cache.values()];
  if (guilds.length === 0) {
    throw new Error("The bot isn't in any server yet. Open the invite link, add it to your server, then try again.");
  }
  if (wanted) {
    const hit = guilds.find((g) => g.id === wanted || g.name.toLowerCase() === wanted.toLowerCase());
    if (!hit) throw new Error(`The bot isn't in a server matching "${wanted}". It is in: ${guilds.map((g) => g.name).join(", ")}`);
    return hit;
  }
  if (guilds.length > 1) {
    throw new Error(`The bot is in several servers (${guilds.map((g) => `${g.name} = ${g.id}`).join(", ")}). Pass --guild <name or id>.`);
  }
  return guilds[0];
}

export async function createWorkspaceChannel(guild: Guild, name: string, categoryId?: string): Promise<TextChannel> {
  const channelName = name.toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "").slice(0, 90) || "workspace";
  return (await guild.channels.create({
    name: channelName,
    type: ChannelType.GuildText,
    parent: categoryId,
    topic: "claudeCord workspace: Claude agents share this channel",
  })) as TextChannel;
}

/** Register a persona for a project folder in a workspace channel and announce it. */
export async function addAgentToChannel(
  client: Client,
  channel: TextChannel,
  persona: string,
  projectPath: string,
  repoUrl: string | null,
): Promise<{ key: string; roleId: string | null }> {
  const name = safePersonaName(persona);
  await scanChannelForPeers(channel);
  if (getDirectoryForChannel(channel.id).some((d) => d.persona.toLowerCase() === name.toLowerCase())) {
    throw new Error(`An agent called "${name}" is already in #${channel.name}. Pick another name with --name.`);
  }
  const key = agentKeyFor(channel.id, name, getAgentsInChannel(channel.id).map((a) => a.channel_id));
  const roleId = await ensureRole(channel.guild, name);
  registerAgent({ key, discordChannelId: channel.id, projectPath, guildId: channel.guild.id, persona: name, repoUrl, roleId });
  await announce(client, getProject(key)!);
  return { key, roleId };
}
