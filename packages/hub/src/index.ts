#!/usr/bin/env node
import { homedir } from "node:os";
import { join } from "node:path";
import { ask, isInteractive } from "@claudecord/protocol";
import { Auth } from "./auth.js";
import {
  HubConfigSchema,
  defaultConfigPath,
  defaultDbPath,
  loadConfig,
  maskToken,
  saveConfig,
  type HubConfig,
} from "./config.js";
import { Db } from "./db.js";
import { DiscordBridge } from "./discord.js";
import { startGateway } from "./gateway.js";
import { Hub } from "./hub.js";
import { createHttpHandler } from "./http.js";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const HELP = `claudecord-hub - the server that connects your machines to Discord

  claudecord-hub setup [--config path]   Create the private config file (asks for each value)
  claudecord-hub check [--config path]   Check the config and that the Discord bot is set up correctly
  claudecord-hub run   [--config path]   Start the hub (the default)

The config is a private file, not environment variables. Default location: ${join(homedir(), ".claudecord", "hub.json")}
`;

function fail(msg: string): never {
  console.error(`error: ${msg}`);
  process.exit(1);
}

const argv = process.argv.slice(2);
const cmd = argv[0] && !argv[0].startsWith("-") ? argv[0] : "run";
const configPath = argv.includes("--config")
  ? (argv[argv.indexOf("--config") + 1] ?? fail("--config needs a path"))
  : defaultConfigPath();

/** The built site sits next to the bundle when published, or in the workspace when developing. */
function findSite(): string | undefined {
  const here = fileURLToPath(new URL(".", import.meta.url));
  return [join(here, "site"), join(here, "../../site/dist")].find((d) => existsSync(join(d, "index.html")));
}

async function setup(): Promise<void> {
  if (!isInteractive()) fail("setup needs a terminal, because it asks for the bot token without showing it.");
  console.log("Creating the hub config. The bot token is not shown as you type.\n");
  const discordToken = await ask("Discord bot token", { hidden: true });
  const guildId = await ask("Discord server (guild) ID");
  const ownerId = await ask("Your Discord user ID");
  const publicUrl = await ask("Public URL of this hub (https://...)", { default: "http://localhost:8787" });
  const port = Number(await ask("Port", { default: "8787" }));
  const dbPath = await ask("Database file", { default: defaultDbPath() });
  const parsed = HubConfigSchema.safeParse({
    discordToken,
    guildId,
    ownerId,
    publicUrl,
    port,
    dbPath,
    categoryName: "claudecord",
  });
  if (!parsed.success) fail(parsed.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join("\n"));
  saveConfig(configPath, parsed.data);
  console.log(`\nSaved to ${configPath} (private to you). Next: claudecord-hub check`);
}

async function start(cfg: HubConfig, opts: { checkOnly?: boolean } = {}): Promise<void> {
  const db = new Db(cfg.dbPath);
  const hub = new Hub(db);
  const auth = new Auth(db);
  const bridge = new DiscordBridge(cfg, hub, auth);
  hub.out = bridge;
  process.on("unhandledRejection", (e) => console.error("unhandled rejection:", e));
  try {
    await bridge.start();
  } catch (e) {
    fail(
      `could not start the Discord bot (${(e as Error).message}). Check the token, the server ID, and that the Message Content intent is on.`,
    );
  }
  if (opts.checkOnly) {
    const problems = await bridge.selfCheck();
    console.log(`Bot logged in as ${bridge.client.user?.tag}. Token ${maskToken(cfg.discordToken)}.`);
    for (const p of problems) console.log(`problem: ${p}`);
    if (!problems.length) console.log("Everything looks right.");
    await bridge.client.destroy();
    process.exit(problems.length ? 1 : 0);
  }
  if (!cfg.publicUrl.startsWith("https://"))
    console.warn(
      "warning: publicUrl is not https. Fine for local use, but devices and the dashboard should reach a public hub over https.",
    );
  setInterval(() => db.pruneExpired(), 10 * 60_000).unref();
  const siteDir = findSite();
  startGateway(hub, cfg.port, createHttpHandler({ hub, auth, publicUrl: cfg.publicUrl, siteDir }));
  console.log(siteDir ? `serving the site from ${siteDir}` : "no built site found, serving the API only");
}

switch (cmd) {
  case "setup":
    await setup();
    break;
  case "check":
    await start(loadConfigOrFail(), { checkOnly: true });
    break;
  case "run":
    await start(loadConfigOrFail());
    break;
  default:
    console.log(HELP);
}

function loadConfigOrFail(): HubConfig {
  try {
    return loadConfig(configPath);
  } catch (e) {
    return fail((e as Error).message);
  }
}
