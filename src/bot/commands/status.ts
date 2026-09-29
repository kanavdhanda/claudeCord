import {
  ChatInputCommandInteraction,
  SlashCommandBuilder,
  TextChannel,
} from "discord.js";
import { getAllProjects, setLiveBoard } from "../../db/database.js";
import { buildStatusEmbed } from "../live-board.js";
import { L } from "../../utils/i18n.js";
import { withHostOption } from "./agent-target.js";

export const data = withHostOption(
  new SlashCommandBuilder()
    .setName("status")
    .setDescription("Show status of all registered project sessions")
    .addBooleanOption((o) =>
      o.setName("live").setDescription("Pin a live-updating status board to this channel"),
    ),
);

export async function execute(
  interaction: ChatInputCommandInteraction,
): Promise<void> {
  const guildId = interaction.guildId!;
  const projects = getAllProjects(guildId);

  if (projects.length === 0) {
    await interaction.editReply({
      content: L("No projects registered. Use `/register` in a channel first.", "등록된 프로젝트가 없습니다. 먼저 채널에서 `/register`를 사용하세요."),
    });
    return;
  }

  const embed = buildStatusEmbed(projects);

  const live = interaction.options.getBoolean("live");
  if (live) {
    const channel = interaction.channel as TextChannel;
    const msg = await channel.send({ embeds: [embed] });
    setLiveBoard(guildId, channel.id, msg.id);
    await interaction.editReply({ content: L("📌 Live status board pinned. It will update automatically.", "📌 실시간 상태 보드가 고정되었습니다.") });
    return;
  }

  await interaction.editReply({ embeds: [embed] });
}
