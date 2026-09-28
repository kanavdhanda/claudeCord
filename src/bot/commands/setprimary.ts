import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getAgentsInChannel, setPrimaryAgent, getPrimaryAgent } from "../../db/database.js";
import { L } from "../../utils/i18n.js";

export const data = new SlashCommandBuilder()
  .setName("setprimary")
  .setDescription("Set this channel's agent as the primary — it receives all unaddressed Discord messages");

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const guildId = interaction.guildId!;
  const channelId = interaction.channelId;

  const agents = getAgentsInChannel(channelId);
  if (agents.length === 0) {
    await interaction.editReply({ content: L("No agent registered in this channel. Use `/register` first.", "이 채널에 등록된 에이전트가 없습니다.") });
    return;
  }

  const agent = agents[0];
  setPrimaryAgent(agent.channel_id, guildId);

  const name = agent.persona ?? agent.project_path.split(/[\\/]/).pop() ?? agent.channel_id;
  await interaction.editReply({
    content: L(
      `✅ **${name}** is now the primary agent. All unaddressed messages in any channel will route here first.`,
      `✅ **${name}**이(가) 기본 에이전트로 설정되었습니다.`,
    ),
  });
}
