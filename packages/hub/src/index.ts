#!/usr/bin/env node
import { loadConfig } from "./config.js";
import { Db } from "./db.js";
import { Hub } from "./hub.js";
import { startGateway } from "./gateway.js";
import { DiscordBridge } from "./discord.js";

const cfg = loadConfig();
const db = new Db(cfg.dbPath);
const hub = new Hub(db);
const bridge = new DiscordBridge(cfg, hub);
hub.out = bridge;
startGateway(hub, cfg.port);
await bridge.start();
