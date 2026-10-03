export interface HubConfig {
  discordToken: string;
  guildId: string;
  ownerId: string;
  categoryName: string;
  port: number;
  dbPath: string;
}

function req(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`Missing env var ${name}`);
  return v;
}

export function loadConfig(): HubConfig {
  return {
    discordToken: req("DISCORD_TOKEN"),
    guildId: req("DISCORD_GUILD_ID"),
    ownerId: req("DISCORD_OWNER_ID"),
    categoryName: process.env.DISCORD_CATEGORY_NAME ?? "claudecord",
    port: Number(process.env.PORT ?? 8787),
    dbPath: process.env.DB_PATH ?? "./hub.db",
  };
}
