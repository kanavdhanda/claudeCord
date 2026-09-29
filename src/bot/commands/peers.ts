import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getProject, recordAudit, setMutePeers } from "../../db/database.js";
import { getConfig } from "../../utils/config.js";
import { getHops, resetHops } from "../../peers/policy.js";
import { peersFor } from "../../agents/persona.js";
import { L } from "../../utils/i18n.js";
import { agentKey, withAgentOption } from "./agent-target.js";

export const data = new SlashCommandBuilder()
  .setName("peers")
  .setDescription("Control how this agent talks to other agents in this channel")
  .addSubcommand((s) => withAgentOption(s.setName("status").setDescription("Show peers, mute state and turn count")))
  .addSubcommand((s) => withAgentOption(s.setName("mute").setDescription("Ignore other agents in this channel (break contact)")))
  .addSubcommand((s) => withAgentOption(s.setName("unmute").setDescription("Allow other agents to message this agent again")))
  .addSubcommand((s) => withAgentOption(s.setName("reset").setDescription("Reset the agent-to-agent turn counter")));

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const key = agentKey(interaction);
  const project = getProject(key);
  if (!project) {
    await interaction.editReply(L("This channel is not registered to any project.", "이 채널은 등록되어 있지 않습니다."));
    return;
  }
  const config = getConfig();
  const sub = interaction.options.getSubcommand();
  const channelId = interaction.channelId; // hop counting is per Discord channel

  if (sub === "mute" || sub === "unmute") {
    setMutePeers(key, sub === "mute");
    recordAudit({ guildId: project.guild_id, channelId: key, userId: interaction.user.id, action: `peers-${sub}` });
    await interaction.editReply(sub === "mute" ? "🔇 Peers muted. This agent ignores other agents here." : "🔊 Peers unmuted.");
    return;
  }
  if (sub === "reset") {
    resetHops(channelId);
    await interaction.editReply("🔄 Turn counter reset.");
    return;
  }

  const peers = peersFor(channelId, key);
  await interaction.editReply({
    embeds: [
      {
        title: "Peers",
        color: project.mute_peers ? 0xff6600 : 0x00aa66,
        fields: [
          { name: "Agent", value: `${project.persona ?? "(no persona)"} on ${config.HOST_NAME}`, inline: true },
          { name: "State", value: project.mute_peers ? "🔇 muted" : "🔊 listening", inline: true },
          { name: "Turns", value: `${getHops(channelId)}/${config.MAX_PEER_HOPS}`, inline: true },
          { name: "Reachable peers here", value: peers.length ? peers.map((p) => p.name).join(", ") : "none", inline: false },
        ],
      },
    ],
  });
}
