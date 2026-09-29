import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getProject, getSession } from "../../db/database.js";
import { getConfig } from "../../utils/config.js";
import { agentKey, withAgentOption } from "./agent-target.js";

export const data = withAgentOption(
  new SlashCommandBuilder().setName("takeover").setDescription("How to continue this agent's session in a terminal"),
);

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const key = agentKey(interaction);
  const project = getProject(key);
  if (!project) {
    await interaction.editReply("No agent is registered in this channel.");
    return;
  }
  const name = project.persona ?? key;
  const session = getSession(key);
  await interaction.editReply({
    embeds: [
      {
        title: `🖥️ Take over ${name}`,
        description:
          `On **${getConfig().HOST_NAME}**, run:\n\`\`\`\nclaudecord attach "${name}"\n\`\`\`` +
          `It opens this agent's real Claude Code session in \`${project.project_path}\`. ` +
          `While you're attached the agent pauses here; when you exit, it carries on from where you left off.` +
          (session?.status === "online" || session?.status === "waiting" ? "\n\n⚠️ It's working right now. Press **Stop** first." : ""),
        color: 0x5865f2,
      },
    ],
  });
}
