#!/usr/bin/env node
import { loadConfig } from "./config.js";
import { Db } from "./db.js";
import { Hub } from "./hub.js";
import { startGateway } from "./gateway.js";
import { DiscordBridge } from "./discord.js";

function fail(msg: string): never {
  console.error(`error: ${msg}`);
  process.exit(1);
}

let cfg;
try {
  cfg = loadConfig();
} catch (e) {
  fail(`${(e as Error).message}. See .env.example.`);
}

const db = new Db(cfg.dbPath);
const hub = new Hub(db);
const bridge = new DiscordBridge(cfg, hub);
hub.out = bridge;

// One bad frame or Discord call must not take the hub down.
process.on("unhandledRejection", (e) => console.error("unhandled rejection:", e));

try {
  await bridge.start();
} catch (e) {
  fail(
    `could not start the Discord bot (${(e as Error).message}). Check DISCORD_TOKEN, DISCORD_GUILD_ID, and that the Message Content intent is enabled for the bot.`,
  );
}
startGateway(hub, cfg.port);
