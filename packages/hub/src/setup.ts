import { HubConfigSchema, type HubConfig } from "./config.js";
import { discover, waitForGuild, type Discovery, type DiscoveryClient, type Guild } from "./discover.js";
import { permissionsInteger, REQUIRED_PERMISSIONS } from "./permissions.js";

export interface SetupIO {
  ask(question: string, opts?: { hidden?: boolean; default?: string }): Promise<string>;
  log(line: string): void;
  makeClient(): DiscoveryClient;
  save(cfg: HubConfig): void;
  /** Opens a URL in the browser. Failing is fine, the link is printed too. */
  open?(url: string): void;
}

export interface SetupDefaults {
  dbPath: string;
  port?: number;
  /** Overrides the owner found from the bot, for a bot owned by someone else. */
  owner?: string;
  waitMs?: number;
  intervalMs?: number;
}

/**
 * Creates the hub config from a bot token and as little else as possible. The bot says who owns it and which servers
 * it is in, so the only other question is where the hub will be reachable.
 */
export async function runSetup(io: SetupIO, d: SetupDefaults): Promise<HubConfig> {
  io.log("Paste your Discord bot token. It is not shown as you type.\n");
  const token = await io.ask("Bot token", { hidden: true });

  let found: Discovery = await discover(token, io.makeClient());
  io.log(`\nLogged in as ${found.botTag}.`);

  if (!found.guilds.length) {
    io.log("\nThe bot is not in any server yet. Open this link, choose your server, and approve:\n");
    io.log(`  ${found.invite}\n`);
    io.log(
      `It asks for ${Object.keys(REQUIRED_PERMISSIONS).length} permissions and not Administrator (permission value ${permissionsInteger()}).`,
    );
    try {
      io.open?.(found.invite);
    } catch {
      /* the link is printed above */
    }
    let shown = false;
    found = await waitForGuild(token, io.makeClient, {
      intervalMs: d.intervalMs,
      timeoutMs: d.waitMs,
      onWait: () => {
        if (!shown) io.log("Waiting for the bot to join a server...");
        shown = true;
      },
    });
    io.log(`The bot joined ${found.guilds.map((g) => g.name).join(", ")}.`);
  }

  let guild: Guild;
  if (found.guilds.length === 1) {
    guild = found.guilds[0]!;
    io.log(`Server: ${guild.name}`);
  } else {
    io.log("\nThe bot is in several servers:");
    found.guilds.forEach((g, i) => io.log(`  ${i + 1}. ${g.name}`));
    const pick = Number(await io.ask("Which one", { default: "1" }));
    guild = found.guilds[pick - 1] ?? found.guilds[0]!;
    io.log(`Server: ${guild.name}`);
  }

  const ownerId = d.owner ?? found.ownerId;
  io.log(`Owner: ${d.owner ? ownerId : found.ownerName}. Only this Discord user can message agents or use commands.`);

  const publicUrl = await io.ask("\nAddress devices and the dashboard will use to reach this hub", {
    default: `http://localhost:${d.port ?? 8787}`,
  });
  const parsed = HubConfigSchema.safeParse({
    discordToken: token,
    guildId: guild.id,
    ownerId,
    publicUrl,
    port: d.port ?? 8787,
    dbPath: d.dbPath,
    categoryName: "claudecord",
  });
  if (!parsed.success) throw new Error(parsed.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join("\n"));
  io.save(parsed.data);
  return parsed.data;
}
