#!/usr/bin/env node
import { homedir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { ask, isInteractive } from "@claudecord/protocol";
import { Auth } from "./auth.js";
import { defaultConfigPath, defaultDbPath, loadConfig, maskToken, saveConfig, type HubConfig } from "./config.js";
import { Db } from "./db.js";
import { DiscordBridge } from "./discord.js";
import { makeClient } from "./discover.js";
import { startGateway } from "./gateway.js";
import { Hub } from "./hub.js";
import { createHttpHandler } from "./http.js";
import { runSetup } from "./setup.js";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const HELP = `claudecord-hub - the server that connects your machines to Discord

  claudecord-hub setup [--config path]   Create the private config file (asks for each value)
  claudecord-hub check [--config path]   Check the config and that the Discord bot is set up correctly
  claudecord-hub run   [--config path]   Start the hub (the default)

Setup asks for one thing, the bot token. Everything else is found from it.\nThe config is a private file, not environment variables. Default location: ${join(homedir(), ".claudecord", "hub.json")}
`;

/** Best effort. The address is always printed as well. */
function openInBrowser(url: string): void {
  const [cmd, args] =
    process.platform === "darwin"
      ? ["open", [url]]
      : process.platform === "win32"
        ? ["cmd", ["/c", "start", "", url]]
        : ["xdg-open", [url]];
  spawn(cmd as string, args as string[], { stdio: "ignore", detached: true })
    .on("error", () => {})
    .unref();
}

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
  try {
    await runSetup(
      {
        ask,
        log: (l) => console.log(l),
        makeClient,
        save: (cfg) => saveConfig(configPath, cfg),
        open: openInBrowser,
      },
      { dbPath: defaultDbPath(), owner: argv.includes("--owner") ? argv[argv.indexOf("--owner") + 1] : undefined },
    );
  } catch (e) {
    fail((e as Error).message);
  }
  console.log(`\nSaved to ${configPath} (private to you). Next: claudecord-hub run`);
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
