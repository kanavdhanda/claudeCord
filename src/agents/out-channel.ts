import type { Client, Guild, MessageCreateOptions, MessageEditOptions } from "discord.js";

/** What the session code needs from "a place to talk". A TextChannel or a persona both fit. */
export interface OutMessage {
  id: string;
  edit(options: string | MessageEditOptions): Promise<OutMessage>;
}

export interface OutChannel {
  /** Agent key (the Discord channel id for the first agent in a channel) */
  id: string;
  name: string;
  guild: Guild;
  client: Client;
  send(options: string | MessageCreateOptions): Promise<OutMessage>;
}
