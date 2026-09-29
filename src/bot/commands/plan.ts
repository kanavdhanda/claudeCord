import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getProject, recordAudit, setPlanFirst } from "../../db/database.js";
import { getConfig } from "../../utils/config.js";
import { agentKey } from "./agent-target.js";

export const data = new SlashCommandBuilder()
  .setName("plan")
  .setDescription("Require an approved plan before Claude starts work in this channel")
  .addStringOption((o) =>
    o
      .setName("mode")
      .setDescription("on = plan and approve first, off = work immediately, default = host setting")
      .setRequired(true)
      .addChoices({ name: "on", value: "on" }, { name: "off", value: "off" }, { name: "default", value: "default" }),
  )
  .addStringOption((o) => o.setName("agent").setDescription("Which agent in this channel (default: the only/first one)"));

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const project = getProject(agentKey(interaction));
  if (!project) {
    await interaction.editReply("This channel is not registered to any project.");
    return;
  }
  const mode = interaction.options.getString("mode", true);
  setPlanFirst(agentKey(interaction), mode === "default" ? null : mode === "on");
  recordAudit({ guildId: project.guild_id, channelId: agentKey(interaction), userId: interaction.user.id, action: `plan-${mode}` });
  const effective = mode === "default" ? getConfig().PLAN_FIRST : mode === "on";
  await interaction.editReply(
    effective
      ? `📋 Plan-first is **on**. Claude proposes a plan and waits for approval; an approved plan covers follow-ups for ${getConfig().PLAN_TTL_MIN} minutes.`
      : "⚡ Plan-first is **off**. Claude starts immediately (destructive commands still ask).",
  );
}
