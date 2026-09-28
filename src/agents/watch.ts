import type { Client } from "discord.js";
import { getEveryProject } from "../db/database.js";
import { isTerminalHeld } from "./terminal.js";
import { outChannelFor } from "./persona.js";

/**
 * Tells the channel when a terminal session takes over an agent or hands it back.
 * The CLI only writes to the database; this loop turns that into a Discord notice.
 */
export function startTerminalWatcher(client: Client): NodeJS.Timeout {
  const state = new Map<string, boolean>();
  for (const p of getEveryProject()) state.set(p.channel_id, isTerminalHeld(p));

  return setInterval(async () => {
    for (const p of getEveryProject()) {
      const held = isTerminalHeld(p);
      const before = state.get(p.channel_id);
      state.set(p.channel_id, held);
      if (before === undefined || before === held) continue;
      const out = await outChannelFor(client, p.channel_id).catch(() => null);
      await out
        ?.send(
          held
            ? "🖥️ **Terminal attached.** Someone is working in this agent's session from a terminal, so it won't take Discord messages until they finish."
            : "↩️ **Terminal detached.** The session is back on Discord and continues from where the terminal left it.",
        )
        .catch(() => {});
    }
  }, 2000);
}
