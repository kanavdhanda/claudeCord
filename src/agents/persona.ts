import {
  type Client,
  type Guild,
  type Message,
  type MessageCreateOptions,
  type MessageEditOptions,
  type TextChannel,
  type Webhook,
} from "discord.js";
import {
  getDirectoryForChannel,
  getEveryProject,
  getProject,
  getWebhookRecord,
  saveWebhookRecord,
  setAgentRole,
  upsertDirectory,
} from "../db/database.js";
import type { Project } from "../db/types.js";
import { getConfig } from "../utils/config.js";
import { discordChannelOf, formatHelloFooter, parseHelloFooter, safePersonaName } from "./identity.js";
import type { OutChannel, OutMessage } from "./out-channel.js";
import type { PeerInfo } from "../claude/context.js";

const webhookCache = new Map<string, Webhook>();

/** One webhook per channel per device; personas speak through it with their own name and avatar. */
export async function ensureWebhook(client: Client, channel: TextChannel): Promise<Webhook> {
  const cached = webhookCache.get(channel.id);
  if (cached) return cached;
  const record = getWebhookRecord(channel.id);
  if (record) {
    try {
      const hook = await client.fetchWebhook(record.webhook_id, record.token);
      webhookCache.set(channel.id, hook);
      return hook;
    } catch {
      // deleted in Discord: fall through and make a new one
    }
  }
  const hook = await channel.createWebhook({ name: "claudeCord" });
  saveWebhookRecord(channel.id, hook.id, hook.token ?? "");
  webhookCache.set(channel.id, hook);
  return hook;
}

/** A mentionable role per persona so people (and other agents) can @ it. Null if not permitted. */
export async function ensureRole(guild: Guild, persona: string): Promise<string | null> {
  const name = `claude · ${persona}`;
  try {
    const existing = guild.roles.cache.find((r) => r.name === name) ?? (await guild.roles.fetch()).find((r) => r.name === name);
    if (existing) return existing.id;
    const role = await guild.roles.create({ name, mentionable: true, color: 0xd97757, reason: "claudeCord agent persona" });
    return role.id;
  } catch (e) {
    console.warn(`[persona] Could not create role "${name}" (needs Manage Roles); falling back to @name routing:`, e instanceof Error ? e.message : e);
    return null;
  }
}

class PersonaMessage implements OutMessage {
  constructor(
    private hook: Webhook,
    public id: string,
  ) {}
  async edit(options: string | MessageEditOptions): Promise<OutMessage> {
    const body = typeof options === "string" ? { content: options } : options;
    await this.hook.editMessage(this.id, body as never);
    return this;
  }
}

/** A persona speaking in a channel. Looks like a channel to the session code. */
export class AgentChannel implements OutChannel {
  readonly id: string;
  readonly name: string;
  readonly guild: Guild;
  readonly client: Client;

  constructor(
    real: TextChannel,
    private hook: Webhook,
    readonly project: Project,
  ) {
    this.id = project.channel_id;
    this.name = real.name;
    this.guild = real.guild;
    this.client = real.client;
  }

  get persona(): string {
    return safePersonaName(this.project.persona ?? "claude");
  }

  async send(options: string | MessageCreateOptions): Promise<OutMessage> {
    const o = typeof options === "string" ? { content: options } : options;
    const sent: Message = await this.hook.send({
      content: o.content,
      embeds: o.embeds,
      components: o.components,
      files: o.files,
      allowedMentions: o.allowedMentions,
      username: this.persona,
      avatarURL: this.project.avatar_url ?? undefined,
    } as never);
    return new PersonaMessage(this.hook, sent.id);
  }
}

/** Build the outgoing channel for an agent key (a persona if it has one, else the plain channel). */
export async function outChannelFor(client: Client, key: string): Promise<OutChannel | null> {
  const project = getProject(key);
  if (!project) return null;
  const real = (await client.channels.fetch(project.discord_channel_id ?? project.channel_id).catch(() => null)) as TextChannel | null;
  if (!real || !("send" in real)) return null;
  if (!project.persona) return real as unknown as OutChannel;
  try {
    return new AgentChannel(real, await ensureWebhook(client, real), project);
  } catch (e) {
    console.warn("[persona] Webhook unavailable (needs Manage Webhooks); speaking as the bot:", e instanceof Error ? e.message : e);
    return real as unknown as OutChannel;
  }
}

/** Announce this agent so other agents (on any device) learn who is in the channel. */
export async function announce(client: Client, project: Project): Promise<void> {
  if (!project.persona) return;
  const real = (await client.channels.fetch(project.discord_channel_id ?? project.channel_id).catch(() => null)) as TextChannel | null;
  if (!real) return;
  const roleId = project.role_id ?? (await ensureRole(real.guild, project.persona));
  if (!roleId) return; // without a role there is nothing peers could mention
  if (roleId !== project.role_id) setAgentRole(project.channel_id, roleId);
  const hook = await ensureWebhook(client, real);
  const channel = new AgentChannel(real, hook, { ...project, role_id: roleId });
  const host = getConfig().HOST_NAME;
  upsertDirectory({ webhook_id: hook.id, persona: project.persona, channel_id: real.id, role_id: roleId, host });
  const sent = await channel.send({
    embeds: [
      {
        title: `🟢 ${project.persona} is online`,
        description: `Running on **${host}** in \`${project.project_path.split(/[\\/]/).pop()}\`.\nMention <@&${roleId}> to give it work.`,
        color: 0xd97757,
        footer: { text: formatHelloFooter({ persona: project.persona, roleId, webhookId: hook.id, host }) },
      },
    ],
    allowedMentions: { parse: [] },
  });
  // Pinned hellos stay findable in busy channels (needs Manage Messages; optional)
  await real.messages.pin(sent.id).catch(() => {});
}

/** Learn agents from a hello message. Returns true if the message was a hello. */
export function learnFromMessage(message: Message): boolean {
  const footer = message.embeds[0]?.footer?.text;
  const hello = parseHelloFooter(footer);
  if (!hello || !message.webhookId || message.webhookId !== hello.webhookId) return false;
  upsertDirectory({ webhook_id: hello.webhookId, persona: hello.persona, channel_id: message.channelId, role_id: hello.roleId, host: hello.host });
  return true;
}

/** On startup, read recent history so agents already in the channel are known. */
export async function scanChannelForPeers(channel: TextChannel): Promise<void> {
  try {
    const pinned = await channel.messages.fetchPins().catch(() => null);
    const pins = pinned ? pinned.items.map((i) => i.message) : [];
    const recent = await channel.messages.fetch({ limit: 100 });
    for (const m of [...pins, ...recent.values()]) learnFromMessage(m);
  } catch (e) {
    console.warn(`[persona] Could not scan #${channel.name} for peers:`, e instanceof Error ? e.message : e);
  }
}

/** Other agents in this channel, as the people the given agent can talk to. */
export function peersFor(discordChannelId: string, selfKey: string): PeerInfo[] {
  const selfRole = getProject(selfKey)?.role_id;
  const here = getConfig().HOST_NAME;
  return getDirectoryForChannel(discordChannelId)
    .filter((d) => d.role_id !== selfRole)
    .map((d) => ({ id: d.role_id, name: d.host === here ? d.persona : `${d.persona} (${d.host})` }));
}

export function realChannelId(key: string): string {
  return discordChannelOf(key);
}

/** On startup: make sure every persona has its webhook and role, learn who else is in each channel, announce if new. */
export async function bootstrapAgents(client: Client, guildAllowed: (guildId: string) => boolean): Promise<void> {
  const byChannel = new Map<string, Project[]>();
  for (const p of getEveryProject()) {
    if (!p.persona || !guildAllowed(p.guild_id)) continue;
    const id = p.discord_channel_id ?? p.channel_id;
    byChannel.set(id, [...(byChannel.get(id) ?? []), p]);
  }
  for (const [channelId, agents] of byChannel) {
    try {
      const real = (await client.channels.fetch(channelId)) as TextChannel | null;
      if (!real || !("messages" in real)) continue;
      await scanChannelForPeers(real);
      const hook = await ensureWebhook(client, real);
      for (const agent of agents) {
        const known = getDirectoryForChannel(channelId).some((d) => d.webhook_id === hook.id && d.persona === agent.persona);
        if (!known || !agent.role_id) await announce(client, agent);
      }
    } catch (e) {
      console.warn(`[persona] Could not set up channel ${channelId}:`, e instanceof Error ? e.message : e);
    }
  }
}
