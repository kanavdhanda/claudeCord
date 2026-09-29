import {
  ChannelType,
  Client,
  EmbedBuilder,
  GatewayIntentBits,
  InteractionContextType,
  REST,
  Routes,
  SlashCommandBuilder,
  WebhookClient,
  type ChatInputCommandInteraction,
  type TextChannel,
} from "discord.js";
import { AdapterId, MAX_FILE_BYTES, autoName } from "@claudecord/protocol";
import type { HubConfig } from "./config.js";
import type { Auth } from "./auth.js";
import { formatCode } from "./auth.js";
import type { Hub, Outbound, PendingAsk } from "./hub.js";
import type { AgentRow } from "./db.js";

const STATUS_LABEL: Record<string, string> = {
  starting: "starting",
  idle: "idle",
  thinking: "thinking",
  executing: "executing",
  waiting_input: "waiting on you",
  paused: "paused",
  limited: "limited",
  offline: "offline",
};

// Reactions: seen means the hub routed the message, accepted means the agent started on it.
const SEEN_REACTION = "\u{1F440}";
const ACCEPTED_REACTION = "\u2705";

export class DiscordBridge implements Outbound {
  client: Client;
  private hooks = new Map<string, WebhookClient>();
  private threads = new Map<string, string>();
  private statusTimers = new Map<string, NodeJS.Timeout>();
  private typingTimer?: NodeJS.Timeout;

  constructor(
    private cfg: HubConfig,
    private hub: Hub,
    private auth: Auth,
  ) {
    this.client = new Client({
      intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildMessages, GatewayIntentBits.MessageContent],
    });
  }

  async start(): Promise<void> {
    this.client.on("messageCreate", (m) => void this.onMessage(m).catch(console.error));
    this.client.on("interactionCreate", (i) => {
      if (i.isChatInputCommand()) void this.onCommand(i).catch(console.error);
    });
    await this.client.login(this.cfg.discordToken);
    await new Promise<void>((r) => this.client.once("clientReady", () => r()));
    await this.registerCommands();
    this.typingTimer = setInterval(() => void this.typing(), 8000);
    console.log(`discord ready as ${this.client.user?.tag}`);
  }

  /** Problems that would stop the bot working, in plain words. Empty when it is set up correctly. */
  async selfCheck(): Promise<string[]> {
    const problems: string[] = [];
    try {
      const app = await this.client.application!.fetch();
      if (!app.flags.has("GatewayMessageContent") && !app.flags.has("GatewayMessageContentLimited")) {
        problems.push("The Message Content intent is off. Turn it on for the bot in the Discord developer portal.");
      }
    } catch {
      problems.push("Could not read the bot's application settings.");
    }
    try {
      const guild = await this.guild();
      const me = await guild.members.fetchMe();
      const need = {
        ViewChannel: "View Channels", ManageChannels: "Manage Channels", ManageWebhooks: "Manage Webhooks",
        SendMessages: "Send Messages", AddReactions: "Add Reactions", AttachFiles: "Attach Files",
        CreatePublicThreads: "Create Public Threads", SendMessagesInThreads: "Send Messages in Threads",
        ReadMessageHistory: "Read Message History", ManageMessages: "Manage Messages (to pin the status board)",
      } as const;
      const missing = Object.entries(need).filter(([k]) => !me.permissions.has(k as keyof typeof need)).map(([, label]) => label);
      if (missing.length) problems.push(`The bot is missing permissions in ${guild.name}: ${missing.join(", ")}.`);
    } catch {
      problems.push(`The bot cannot see the server with ID ${this.cfg.guildId}. Check the ID and that the bot was invited to it.`);
    }
    return problems;
  }

  // Outbound

  private async guild() {
    return this.client.guilds.fetch(this.cfg.guildId);
  }

  async ensureProject(project: string): Promise<void> {
    if (this.hub.db.getProject(project)) return;
    const guild = await this.guild();
    const channels = await guild.channels.fetch();
    let category = channels.find((c) => c?.type === ChannelType.GuildCategory && c.name === this.cfg.categoryName);
    if (!category) {
      category = await guild.channels.create({ name: this.cfg.categoryName, type: ChannelType.GuildCategory });
    }
    const chName = slug(project);
    let ch = channels.find(
      (c) => c?.type === ChannelType.GuildText && c.name === chName && c.parentId === category!.id,
    ) as TextChannel | undefined;
    if (!ch) {
      ch = await guild.channels.create({ name: chName, type: ChannelType.GuildText, parent: category.id });
    }
    const hook = await ch.createWebhook({ name: "claudecord" });
    this.hub.db.saveProject({
      name: project,
      channel_id: ch.id,
      webhook_id: hook.id,
      webhook_token: hook.token ?? null,
      status_message_id: null,
    });
  }

  private hook(project: string): WebhookClient {
    let h = this.hooks.get(project);
    if (!h) {
      const p = this.hub.db.getProject(project);
      if (!p?.webhook_id || !p.webhook_token) throw new Error(`no webhook for ${project}`);
      h = new WebhookClient({ id: p.webhook_id, token: p.webhook_token });
      this.hooks.set(project, h);
    }
    return h;
  }

  private async channel(project: string): Promise<TextChannel> {
    const p = this.hub.db.getProject(project);
    if (!p) throw new Error(`unknown project ${project}`);
    return (await this.client.channels.fetch(p.channel_id)) as TextChannel;
  }

  private async threadId(project: string, thread?: string): Promise<string | undefined> {
    if (!thread) return undefined;
    const key = `${project}:${thread}`;
    const known = this.threads.get(key);
    if (known) return known;
    const ch = await this.channel(project);
    const active = await ch.threads.fetchActive();
    let t = active.threads.find((x) => x.name === thread);
    if (!t) t = await ch.threads.create({ name: thread.slice(0, 90), autoArchiveDuration: 1440 });
    this.threads.set(key, t.id);
    return t.id;
  }

  private identity(a: AgentRow) {
    const id: { username: string; avatarURL?: string } = { username: `${a.name} (${a.adapter})`.slice(0, 80) };
    // Avatars come from a third party that would learn agent names, so they are opt-in.
    if (this.cfg.avatarTemplate) id.avatarURL = this.cfg.avatarTemplate.replace("{name}", encodeURIComponent(a.name));
    return id;
  }

  async post(project: string, agent: AgentRow, text: string, thread?: string): Promise<void> {
    const threadId = await this.threadId(project, thread);
    for (const chunk of chunks(text)) {
      await this.hook(project).send({
        content: chunk,
        ...this.identity(agent),
        threadId,
        allowedMentions: { users: [this.cfg.ownerId] },
      });
    }
  }

  async postAsk(project: string, agent: AgentRow, ask: PendingAsk): Promise<void> {
    const threadId = await this.threadId(project, ask.thread);
    const opts = ask.options?.length ? "\n\n" + ask.options.map((o, i) => `${i + 1}. ${o}`).join("\n") : "";
    const embed = new EmbedBuilder()
      .setTitle("Question")
      .setDescription((ask.question + opts).slice(0, 4000))
      .setFooter({ text: `Reply here, or mention @${agent.name}` });
    await this.hook(project).send({
      content: `<@${this.cfg.ownerId}>`,
      embeds: [embed],
      ...this.identity(agent),
      threadId,
      allowedMentions: { users: [this.cfg.ownerId] },
    });
  }

  async postReport(project: string, agent: AgentRow, title: string, summary: string, artifacts?: string[]): Promise<void> {
    const embed = new EmbedBuilder().setTitle(title.slice(0, 250)).setDescription(summary.slice(0, 4000));
    if (artifacts?.length) embed.addFields({ name: "Artifacts", value: artifacts.join("\n").slice(0, 1000) });
    await this.hook(project).send({
      content: `<@${this.cfg.ownerId}> [STATUS: COMPLETE]`,
      embeds: [embed],
      ...this.identity(agent),
      allowedMentions: { users: [this.cfg.ownerId] },
    });
  }

  async postFile(project: string, agent: AgentRow, name: string, data: Buffer, caption?: string, thread?: string): Promise<void> {
    const threadId = await this.threadId(project, thread);
    await this.hook(project).send({
      content: caption?.slice(0, 1900),
      files: [{ attachment: data, name }],
      ...this.identity(agent),
      threadId,
      allowedMentions: { users: [this.cfg.ownerId] },
    });
  }

  async confirm(project: string, ref: string, agentName: string): Promise<void> {
    const [channelId, messageId] = ref.split(":");
    if (!channelId || !messageId) return;
    const ch = await this.client.channels.fetch(channelId);
    if (!ch?.isTextBased()) return;
    const msg = await ch.messages.fetch(messageId);
    await msg.reactions.cache.get(SEEN_REACTION)?.users.remove(this.client.user!.id).catch(() => {});
    await msg.react(ACCEPTED_REACTION);
    void agentName;
  }

  async notice(project: string, text: string, mention = false): Promise<void> {
    const ch = await this.channel(project);
    await ch.send({
      content: `${mention ? `<@${this.cfg.ownerId}> ` : ""}${text}`,
      allowedMentions: { users: mention ? [this.cfg.ownerId] : [] },
    });
  }

  refreshStatus(project: string): void {
    clearTimeout(this.statusTimers.get(project));
    this.statusTimers.set(
      project,
      setTimeout(() => void this.renderStatus(project).catch(console.error), 1500),
    );
  }

  private statusText(project: string): string {
    const agents = this.hub.db.agentsOfProject(project);
    if (!agents.length) return "No agents.";
    return agents
      .map((a) => {
        const s = this.hub.status.get(a.agent_id);
        const detail = s?.detail ? ` (${s.detail})` : "";
        return `${a.name}${a.is_lead ? " [lead]" : ""} - ${a.adapter}${a.model ? `/${a.model}` : ""} on ${a.node_name}: ${STATUS_LABEL[s?.status ?? "offline"]}${detail}`;
      })
      .join("\n");
  }

  private taskBoard(project: string): string {
    const rows = this.hub.db.tasksOfProject(project);
    if (!rows.length) return "No tasks yet.";
    const name = (id: string) => this.hub.agent(id)?.name ?? id.split("/").pop();
    return rows
      .map((t) => `${t.id}  ${t.state.padEnd(8)} ${name(t.to_agent)}: ${t.text.slice(0, 80)}${t.summary ? `  -> ${t.summary.slice(0, 80)}` : ""}`)
      .join("\n");
  }

  private async renderStatus(project: string): Promise<void> {
    const p = this.hub.db.getProject(project);
    if (!p) return;
    const ch = await this.channel(project);
    const content = "Status\n```\n" + this.statusText(project) + "\n```";
    if (p.status_message_id) {
      try {
        const msg = await ch.messages.fetch(p.status_message_id);
        await msg.edit(content);
        return;
      } catch {
        /* message gone, create a new one */
      }
    }
    const msg = await ch.send(content);
    await msg.pin().catch(() => {});
    this.hub.db.saveProject({ ...p, status_message_id: msg.id });
  }

  private async typing(): Promise<void> {
    const busy = new Set<string>();
    for (const a of this.hub.db.allAgents()) {
      const s = this.hub.status.get(a.agent_id)?.status;
      if (s === "thinking" || s === "executing") busy.add(a.project);
    }
    for (const p of busy) await (await this.channel(p)).sendTyping().catch(() => {});
  }

  // Inbound

  private async onMessage(m: import("discord.js").Message): Promise<void> {
    if (m.author.bot || m.webhookId || m.author.id !== this.cfg.ownerId) return;
    if (!m.content.trim() && !m.attachments.size) return;
    let channelId = m.channelId;
    let thread: string | undefined;
    if (m.channel.isThread() && m.channel.parentId) {
      channelId = m.channel.parentId;
      thread = m.channel.name;
    }
    const p = this.hub.db.projectByChannel(channelId);
    if (!p) return;
    if (m.attachments.size) {
      await this.forwardAttachments(m, p.name, thread);
      if (!m.content.trim()) return;
    }
    const res = this.hub.humanMessage(p.name, m.content, thread, `${m.channelId}:${m.id}`);
    if (!res.targets.length) {
      await m.reply({ content: "No agents are connected to this project.", allowedMentions: { repliedUser: false } });
      return;
    }
    if (res.offline.length === res.targets.length) {
      await m.reply({ content: `${res.offline.join(", ")} not connected, so nothing was delivered.`, allowedMentions: { repliedUser: false } });
      return;
    }
    await m.react(SEEN_REACTION).catch(() => {});
    for (const h of res.held) {
      await m.reply({ content: `${h.name} is ${h.why}. Your message is queued and will be picked up when it can.`, allowedMentions: { repliedUser: false } });
    }
  }

  private async forwardAttachments(m: import("discord.js").Message, project: string, thread?: string): Promise<void> {
    let delivered: string[] = [];
    for (const att of m.attachments.values()) {
      if (att.size > MAX_FILE_BYTES) {
        await m.reply({ content: `${att.name} is over the ${MAX_FILE_BYTES / 1048576} MB limit.`, allowedMentions: { repliedUser: false } });
        continue;
      }
      if (!isDiscordCdn(att.url)) continue;
      const res = await fetch(att.url, { redirect: "error" });
      if (!res.ok) continue;
      delivered = this.hub.sendFile(project, m.content, att.name, Buffer.from(await res.arrayBuffer()), thread);
    }
    if (delivered.length) await m.react(SEEN_REACTION).catch(() => {});
    else await m.reply({ content: "No agents are connected to this project.", allowedMentions: { repliedUser: false } });
  }

  // Slash commands

  private async registerCommands(): Promise<void> {
    const adapters = AdapterId.options.map((o) => ({ name: o, value: o }));
    const cmds = [
      new SlashCommandBuilder().setName("killall").setDescription("Stop all agents")
        .addStringOption((o) => o.setName("project").setDescription("Limit to a project")),
      new SlashCommandBuilder().setName("stop").setDescription("Stop one agent")
        .addStringOption((o) => o.setName("agent").setDescription("Agent name").setRequired(true)),
      new SlashCommandBuilder().setName("pause").setDescription("Hold message delivery")
        .addStringOption((o) => o.setName("agent").setDescription("One agent, default all in project")),
      new SlashCommandBuilder().setName("resume").setDescription("Resume message delivery")
        .addStringOption((o) => o.setName("agent").setDescription("One agent, default all in project")),
      new SlashCommandBuilder().setName("spawn").setDescription("Add an agent to this project")
        .addStringOption((o) => o.setName("node").setDescription("Device name").setRequired(true))
        .addStringOption((o) => o.setName("name").setDescription("Agent name, auto if empty"))
        .addStringOption((o) => o.setName("adapter").setDescription("Agent type").addChoices(...adapters))
        .addStringOption((o) => o.setName("model").setDescription("Model"))
        .addStringOption((o) => o.setName("role").setDescription("Role")),
      new SlashCommandBuilder().setName("agents").setDescription("List agents"),
      new SlashCommandBuilder().setName("status").setDescription("Show project status"),
      new SlashCommandBuilder().setName("tasks").setDescription("Show the task board for this project"),
      new SlashCommandBuilder().setName("lead").setDescription("Set the lead agent")
        .addStringOption((o) => o.setName("agent").setDescription("Agent name").setRequired(true)),
      new SlashCommandBuilder().setName("connect").setDescription("Get a one-time code to connect a machine"),
      new SlashCommandBuilder().setName("dashboard").setDescription("Get a one-time link to open the dashboard"),
      new SlashCommandBuilder().setName("devices").setDescription("List connected machines"),
      new SlashCommandBuilder().setName("revoke").setDescription("Revoke a device token")
        .addStringOption((o) => o.setName("device").setDescription("Device name").setRequired(true)),
    ].map((c) => c.setDefaultMemberPermissions(0).setContexts(InteractionContextType.Guild).toJSON());
    const rest = new REST().setToken(this.cfg.discordToken);
    await rest.put(Routes.applicationGuildCommands(this.client.user!.id, this.cfg.guildId), { body: cmds });
  }

  private projectOf(i: ChatInputCommandInteraction): string | undefined {
    const id = i.channel?.isThread() ? i.channel.parentId : i.channelId;
    return id ? this.hub.db.projectByChannel(id)?.name : undefined;
  }

  private async onCommand(i: ChatInputCommandInteraction): Promise<void> {
    if (i.user.id !== this.cfg.ownerId) {
      await i.reply({ content: "Not allowed.", ephemeral: true });
      return;
    }
    const project = this.projectOf(i);
    const s = (n: string) => i.options.getString(n) ?? undefined;
    const reply = (content: string, ephemeral = false) => i.reply({ content, ephemeral });

    switch (i.commandName) {
      case "killall": {
        const n = this.hub.killall(s("project") ?? project);
        return void (await reply(`Stopping ${n} agent(s).`));
      }
      case "stop": {
        if (!project) return void (await reply("Run this inside a project channel.", true));
        const ok = this.hub.stop(project, s("agent")!);
        return void (await reply(ok ? "Stopping." : "Agent not found or node offline."));
      }
      case "pause":
      case "resume": {
        const n = this.hub.hold(i.commandName === "pause", project, s("agent"));
        return void (await reply(`${i.commandName === "pause" ? "Paused" : "Resumed"} ${n} agent(s).`));
      }
      case "spawn": {
        if (!project) return void (await reply("Run this inside a project channel.", true));
        const taken = new Set(this.hub.db.agentsOfProject(project).map((a) => a.name));
        const name = s("name") ?? autoName(taken);
        const spec = {
          agentId: `${project}/${name}`,
          name,
          project,
          adapter: (s("adapter") ?? "claude") as AdapterId,
          model: s("model"),
          role: s("role"),
        };
        const ok = this.hub.spawn(s("node")!, spec);
        return void (await reply(ok ? `Spawning ${name}.` : `Node ${s("node")} is not connected.`));
      }
      case "agents":
      case "status": {
        const text = project ? this.statusText(project) : "Run this inside a project channel.";
        return void (await reply("```\n" + text + "\n```"));
      }
      case "tasks": {
        if (!project) return void (await reply("Run this inside a project channel.", true));
        return void (await reply("```\n" + this.taskBoard(project) + "\n```"));
      }
      case "lead": {
        if (!project) return void (await reply("Run this inside a project channel.", true));
        const a = this.hub.findByName(project, s("agent")!);
        if (!a) return void (await reply("Agent not found.", true));
        this.hub.setLead(project, a.agent_id);
        return void (await reply(`${a.name} is now lead.`));
      }
      case "connect": {
        const { code, expiresInMs } = this.auth.createPairCode();
        const mins = Math.round(expiresInMs / 60_000);
        return void (await reply(
          `Run this on the machine you want to connect. It works once and expires in ${mins} minutes.\n\`\`\`\nnpx claudecord login ${this.cfg.publicUrl} ${code}\n\`\`\``,
          true,
        ));
      }
      case "dashboard": {
        const { token, expiresInMs } = this.auth.createLoginToken();
        const url = `${this.cfg.publicUrl.replace(/\/$/, "")}/dashboard/login?t=${token}`;
        return void (await reply(`Open this link to sign in. It works once and expires in ${Math.round(expiresInMs / 60_000)} minutes.\n${url}`, true));
      }
      case "devices": {
        const rows = this.hub.db.listDevices().filter((d) => !d.revoked);
        const text = rows.length
          ? rows.map((d) => `${d.node_name.padEnd(20)} ${this.hub.nodes.has(d.node_name) ? "online" : "offline"}  ${this.hub.db.agentsOfNode(d.node_name).length} agent(s)`).join("\n")
          : "No devices yet. Run /connect.";
        return void (await reply("```\n" + text + "\n```", true));
      }
      case "revoke": {
        const ok = this.hub.db.revokeToken(s("device")!);
        return void (await reply(ok ? "Revoked." : "No such device.", true));
      }
    }
  }
}

/** Attachment URLs come from Discord, but only ever fetch from its own CDN. */
export function isDiscordCdn(url: string): boolean {
  try {
    const u = new URL(url);
    return u.protocol === "https:" && (u.hostname === "cdn.discordapp.com" || u.hostname === "media.discordapp.net");
  } catch {
    return false;
  }
}

function slug(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9-_]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 90) || "project";
}

function* chunks(text: string, size = 1900): Generator<string> {
  for (let i = 0; i < text.length; i += size) yield text.slice(i, i + size);
}
