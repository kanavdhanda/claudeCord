import { Client, EmbedBuilder, TextChannel } from "discord.js";
import { getAllProjects, getSession, getLiveBoard, clearLiveBoard } from "../db/database.js";
import type { Project } from "../db/types.js";

const STATUS_EMOJI: Record<string, string> = {
  online: "🟢",
  waiting: "🟡",
  idle: "⚪",
  offline: "🔴",
};

export function buildStatusEmbed(projects: Project[]): EmbedBuilder {
  const embed = new EmbedBuilder()
    .setTitle("Claude Code — Live Status")
    .setColor(0x7c3aed)
    .setTimestamp();

  for (const project of projects) {
    const session = getSession(project.channel_id);
    const status = session?.status ?? "offline";
    const emoji = STATUS_EMOJI[status] ?? "🔴";
    const lastActivity = session?.last_activity ?? "never";
    const name = project.persona ?? project.project_path.split(/[\\/]/).pop() ?? project.channel_id;
    embed.addFields({
      name: `${emoji} ${name}`,
      value: [
        `<#${project.channel_id}>  ·  \`${project.project_path}\``,
        `**${status}**  ·  last: ${lastActivity}`,
      ].join("\n"),
      inline: false,
    });
  }

  if (projects.length === 0) embed.setDescription("No agents registered.");
  return embed;
}

export async function refreshLiveBoard(client: Client, guildId: string): Promise<void> {
  const board = getLiveBoard(guildId);
  if (!board) return;

  try {
    const channel = await client.channels.fetch(board.channel_id);
    if (!channel || !(channel instanceof TextChannel)) return;
    const message = await channel.messages.fetch(board.message_id);
    const projects = getAllProjects(guildId);
    await message.edit({ embeds: [buildStatusEmbed(projects)] });
  } catch (e: unknown) {
    const code = (e as { code?: number }).code;
    if (code === 10008 || code === 10003) {
      // Message or channel deleted — stop tracking
      clearLiveBoard(guildId);
    }
  }
}
