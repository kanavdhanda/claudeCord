import {
  ChatInputCommandInteraction,
  SlashCommandBuilder,
} from "discord.js";
import { getProject, setAutoApprove } from "../../db/database.js";
import { L } from "../../utils/i18n.js";
import { agentKey, withAgentOption } from "./agent-target.js";

export const data = withAgentOption(new SlashCommandBuilder()
  .setName("auto-approve")
  .setDescription("Toggle auto-approve mode for tool use in this channel")
  .addStringOption((opt) =>
    opt
      .setName("mode")
      .setDescription("on or off")
      .setRequired(true)
      .addChoices(
        { name: "on", value: "on" },
        { name: "off", value: "off" },
      ),
  ));

export async function execute(
  interaction: ChatInputCommandInteraction,
): Promise<void> {
  const channelId = agentKey(interaction);
  const mode = interaction.options.getString("mode", true);
  const project = getProject(channelId);

  if (!project) {
    await interaction.editReply({
      content: L("This channel is not registered to any project.", "이 채널은 어떤 프로젝트에도 등록되어 있지 않습니다."),
    });
    return;
  }

  const enabled = mode === "on";
  setAutoApprove(channelId, enabled);

  await interaction.editReply({
    embeds: [
      {
        title: L(`Auto-approve: ${enabled ? "ON" : "OFF"}`, `자동 승인: ${enabled ? "ON" : "OFF"}`),
        description: enabled
          ? L("Claude will automatically approve all tool uses (Edit, Write, Bash, etc.)", "Claude가 모든 도구 사용을 자동으로 승인합니다 (Edit, Write, Bash 등)")
          : L("Claude will ask for approval before using tools", "Claude가 도구 사용 전에 승인을 요청합니다"),
        color: enabled ? 0x00ff00 : 0xff6600,
      },
    ],
  });
}
