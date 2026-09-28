import type { ChatInputCommandInteraction } from "discord.js";
import { getAgentsInChannel } from "../../db/database.js";

/**
 * Which local agent a slash command acts on: the `agent` option if given, otherwise the only
 * (or first) agent registered in this channel. Falls back to the channel id, which resolves to
 * "not registered" for commands that check.
 */
export function agentKey(interaction: ChatInputCommandInteraction): string {
  const agents = getAgentsInChannel(interaction.channelId);
  let wanted: string | null = null;
  try {
    wanted = interaction.options.getString("agent");
  } catch {
    // command has no agent option
  }
  if (wanted) {
    const hit = agents.find((a) => a.persona?.toLowerCase() === wanted!.toLowerCase());
    if (hit) return hit.channel_id;
  }
  return agents[0]?.channel_id ?? interaction.channelId;
}

/** Add the optional `agent` option (persona name) to a command or subcommand builder (mutates it). */
export function withAgentOption<T>(builder: T): T {
  (builder as unknown as { addStringOption: (fn: (o: any) => any) => unknown }).addStringOption((o) =>
    o.setName("agent").setDescription("Which agent in this channel (default: the only/first one)"),
  );
  return builder;
}

/** Add the optional `host` option: run this command on a specific device. */
export function withHostOption<T>(builder: T): T {
  (builder as unknown as { addStringOption: (fn: (o: any) => any) => unknown }).addStringOption((o) =>
    o.setName("host").setDescription("Run on this device (default: the primary device)"),
  );
  return builder;
}
