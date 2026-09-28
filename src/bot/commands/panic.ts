import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getAllProjects, recordAudit, setMutePeers } from "../../db/database.js";
import { sessionManager } from "../../claude/session-manager.js";
import { resetHops } from "../../peers/policy.js";
import { withHostOption } from "./agent-target.js";

export const data = withHostOption(new SlashCommandBuilder()
  .setName("panic")
  .setDescription("Emergency stop: halt every session on this host and mute all peers"));

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const guildId = interaction.guildId!;
  const stopped = await sessionManager.stopAll();
  const projects = getAllProjects(guildId);
  for (const p of projects) {
    setMutePeers(p.channel_id, true);
    resetHops(p.channel_id);
  }
  recordAudit({ guildId, channelId: interaction.channelId, userId: interaction.user.id, action: "panic", detail: `stopped ${stopped}` });
  await interaction.editReply(
    `🛑 Stopped ${stopped} running session(s) and muted peers in ${projects.length} channel(s). Use \`/peers unmute\` per channel to resume agent-to-agent messaging.`,
  );
}
