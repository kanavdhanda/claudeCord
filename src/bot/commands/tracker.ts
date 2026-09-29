import { ChatInputCommandInteraction, SlashCommandBuilder } from "discord.js";
import { getRecentAudit, getUsageSummary } from "../../db/database.js";
import { getConfig } from "../../utils/config.js";
import { withHostOption } from "./agent-target.js";

export const data = withHostOption(new SlashCommandBuilder()
  .setName("tracker")
  .setDescription("Who used this agent: turns, time and cost per user and channel")
  .addStringOption((o) =>
    o
      .setName("period")
      .setDescription("Time window (default: today)")
      .addChoices({ name: "today", value: "day" }, { name: "7 days", value: "week" }, { name: "30 days", value: "month" }),
  )
  .addBooleanOption((o) => o.setName("audit").setDescription("Also show the recent audit log")));

const HOURS: Record<string, number> = { day: 24, week: 24 * 7, month: 24 * 30 };

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  const period = interaction.options.getString("period") ?? "day";
  const since = new Date(Date.now() - HOURS[period] * 3_600_000).toISOString().replace("T", " ").slice(0, 19);
  const guildId = interaction.guildId!;
  const rows = getUsageSummary(guildId, since);
  const showCost = getConfig().SHOW_COST;

  const total = rows.reduce((a, r) => ({ turns: a.turns + r.turns, cost: a.cost + r.cost_usd, ms: a.ms + r.duration_ms }), { turns: 0, cost: 0, ms: 0 });
  const lines = rows.slice(0, 15).map((r) => {
    const mins = (r.duration_ms / 60_000).toFixed(1);
    return `<@${r.user_id}> in <#${r.channel_id}> · ${r.turns} turns · ${mins} min${showCost ? ` · $${r.cost_usd.toFixed(2)}` : ""}`;
  });

  const embeds: object[] = [
    {
      title: `Usage · ${period === "day" ? "last 24h" : period === "week" ? "last 7 days" : "last 30 days"} · ${getConfig().HOST_NAME}`,
      description: lines.length ? lines.join("\n") : "No activity recorded.",
      color: 0x5865f2,
      footer: { text: `Total: ${total.turns} turns · ${(total.ms / 60_000).toFixed(1)} min${showCost ? ` · $${total.cost.toFixed(2)}` : ""}` },
    },
  ];

  if (interaction.options.getBoolean("audit")) {
    const audit = getRecentAudit(guildId, 15);
    embeds.push({
      title: "Recent audit log",
      description: audit.length
        ? audit.map((a) => `\`${a.ts.slice(5, 16)}\` <@${a.user_id ?? "0"}> **${a.action}**${a.detail ? ` — ${a.detail.slice(0, 60)}` : ""}`).join("\n")
        : "Empty.",
      color: 0x99aab5,
    });
  }
  await interaction.editReply({ embeds: embeds as never });
}
