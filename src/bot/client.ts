import {
  Client,
  GatewayIntentBits,
  REST,
  Routes,
  Collection,
  type ChatInputCommandInteraction,
  type Interaction,
} from "discord.js";
import { allowedGuildIds, getConfig } from "../utils/config.js";
import { handleMessage } from "./handlers/message.js";
import { handleButtonInteraction, handleModalSubmit, handleSelectMenuInteraction } from "./handlers/interaction.js";
import { isAdmin } from "../security/guard.js";
import { getAgentsInChannel, getDirectoryForChannel, isPrimaryDevice } from "../db/database.js";
import { bootstrapAgents } from "../agents/persona.js";
import { startTerminalWatcher } from "../agents/watch.js";
import { L } from "../utils/i18n.js";

// Import commands
import * as registerCmd from "./commands/register.js";
import * as unregisterCmd from "./commands/unregister.js";
import * as statusCmd from "./commands/status.js";
import * as stopCmd from "./commands/stop.js";
import * as autoApproveCmd from "./commands/auto-approve.js";
import * as sessionsCmd from "./commands/sessions.js";
import * as clearSessionsCmd from "./commands/clear-sessions.js";
import * as lastCmd from "./commands/last.js";
import * as queueCmd from "./commands/queue.js";
import * as usageCmd from "./commands/usage.js";
import * as projectCmd from "./commands/project.js";
import * as peersCmd from "./commands/peers.js";
import * as panicCmd from "./commands/panic.js";
import * as trackerCmd from "./commands/tracker.js";
import * as planCmd from "./commands/plan.js";
import * as takeoverCmd from "./commands/takeover.js";
import * as setprimaryCmd from "./commands/setprimary.js";

const commands = [registerCmd, unregisterCmd, statusCmd, stopCmd, autoApproveCmd, sessionsCmd, clearSessionsCmd, lastCmd, queueCmd, usageCmd, projectCmd, peersCmd, panicCmd, trackerCmd, planCmd, takeoverCmd, setprimaryCmd];
const commandMap = new Collection<
  string,
  { execute: (interaction: ChatInputCommandInteraction) => Promise<void> }
>();

for (const cmd of commands) {
  commandMap.set(cmd.data.name, cmd);
}

/** Commands that act on an agent that lives on one particular device. */
const AGENT_COMMANDS = new Set(["stop", "plan", "peers", "auto-approve", "unregister", "last", "clear-sessions", "queue", "sessions", "takeover"]);

/**
 * Devices can share one bot token, so every device receives every command. Decide if this one answers.
 * - `host:` names a device: only that device answers.
 * - Agent commands: the device that runs an agent here answers (the primary one if nobody does).
 * - Everything else: the primary device answers.
 */
export function shouldHandleCommand(interaction: ChatInputCommandInteraction): boolean {
  const config = getConfig();
  let host: string | null = null;
  try {
    host = interaction.options.getString("host");
  } catch {
    // no host option on this command
  }
  if (host) return host.toLowerCase() === config.HOST_NAME.toLowerCase();
  if (AGENT_COMMANDS.has(interaction.commandName)) {
    if (getAgentsInChannel(interaction.channelId).length > 0) return true;
    return isPrimaryDevice() && getDirectoryForChannel(interaction.channelId).length === 0;
  }
  return isPrimaryDevice();
}

export async function startBot(): Promise<Client> {
  const config = getConfig();

  const client = new Client({
    intents: [
      GatewayIntentBits.Guilds,
      GatewayIntentBits.GuildMessages,
      GatewayIntentBits.MessageContent,
    ],
  });

  // Servers this bot may work in: the configured list, or every server it has joined
  const guildAllowed = (guildId: string | null | undefined): boolean => {
    if (!guildId) return false;
    const list = allowedGuildIds(config);
    return list.length === 0 || list.includes(guildId);
  };

  const registerCommandsIn = async (guildId: string) => {
    if (!guildAllowed(guildId)) return;
    try {
      const rest = new REST({ version: "10" }).setToken(config.DISCORD_BOT_TOKEN);
      const commandData = commands.map((c) => c.data.toJSON());
      await rest.put(
        Routes.applicationGuildCommands((await rest.get(Routes.currentApplication()) as { id: string }).id, guildId),
        { body: commandData },
      );
      console.log(`Registered ${commandData.length} slash commands in guild ${guildId}`);
    } catch (error) {
      console.error(`Failed to register slash commands in guild ${guildId}:`, error);
    }
  };

  client.on("clientReady", async () => {
    console.log(`Bot logged in as ${client.user?.tag} (host: ${config.HOST_NAME})`);
    for (const guild of client.guilds.cache.values()) await registerCommandsIn(guild.id);
    await bootstrapAgents(client, (id) => guildAllowed(id));
    startTerminalWatcher(client);
  });

  // Invited to a new server: make commands available there straight away
  client.on("guildCreate", (guild) => {
    void registerCommandsIn(guild.id);
  });

  // Handle interactions (slash commands + buttons)
  client.on("interactionCreate", async (interaction: Interaction) => {
    try {
      if (interaction.isAutocomplete()) {
        const command = commandMap.get(interaction.commandName);
        if (command && "autocomplete" in command) {
          await (command as any).autocomplete(interaction);
        }
        return;
      }

      if (interaction.guildId && !guildAllowed(interaction.guildId)) return;

      if (interaction.isModalSubmit()) {
        await handleModalSubmit(interaction);
        return;
      }

      if (interaction.isChatInputCommand()) {
        if (!shouldHandleCommand(interaction)) return; // another device answers

        // Auth check
        if (!isAdmin(interaction.user.id)) {
          await interaction.reply({
            content: L("You are not authorized to use this bot.", "이 봇을 사용할 권한이 없습니다."),
            flags: ["Ephemeral"],
          });
          return;
        }

        // Defer reply to avoid 3-second timeout
        await interaction.deferReply();

        const command = commandMap.get(interaction.commandName);
        if (command) {
          await command.execute(interaction);
        }
      } else if (interaction.isButton()) {
        await handleButtonInteraction(interaction);
      } else if (interaction.isStringSelectMenu()) {
        await handleSelectMenuInteraction(interaction);
      }
    } catch (error) {
      console.error("Interaction error:", error);
      const content = L("An error occurred while processing your command.", "명령을 처리하는 중 오류가 발생했습니다.");
      try {
        if (interaction.isRepliable()) {
          if (interaction.replied || interaction.deferred) {
            await interaction.followUp({ content, flags: ["Ephemeral"] });
          } else {
            await interaction.reply({ content, flags: ["Ephemeral"] });
          }
        }
      } catch {
        // ignore follow-up errors
      }
    }
  });

  // Handle messages (wrapped with error handler to prevent silent hangs)
  client.on("messageCreate", async (message) => {
    if (message.guildId && !guildAllowed(message.guildId)) return;
    try {
      await handleMessage(message);
    } catch (error) {
      console.error("messageCreate error:", error);
      try {
        if (message.channel.isSendable()) {
          await message.reply(L("An error occurred while processing your message.", "메시지를 처리하는 중 오류가 발생했습니다."));
        }
      } catch {
        // ignore reply error
      }
    }
  });

  // Discord.js error handlers — prevent silent disconnects
  client.on("error", (error) => {
    console.error("Discord client error:", error);
  });

  client.on("warn", (warning) => {
    console.warn("Discord warning:", warning);
  });

  client.on("shardDisconnect", (event, shardId) => {
    console.warn(`Shard ${shardId} disconnected (code ${event.code}). Reconnecting...`);
  });

  client.on("shardReconnecting", (shardId) => {
    console.log(`Shard ${shardId} reconnecting...`);
  });

  client.on("shardResume", (shardId, replayedEvents) => {
    console.log(`Shard ${shardId} resumed (${replayedEvents} events replayed)`);
  });

  client.on("shardError", (error, shardId) => {
    console.error(`Shard ${shardId} error:`, error);
  });

  // Login with retry (network may not be ready on boot)
  await loginWithRetry(client, config.DISCORD_BOT_TOKEN);
  return client;
}

async function loginWithRetry(client: Client, token: string): Promise<void> {
  const delays = [5, 10, 15, 30, 30, 30]; // seconds — escalating, then steady 30s
  let attempt = 0;

  while (true) {
    try {
      await client.login(token);
      if (attempt > 0) {
        console.log(`Discord login successful after ${attempt} retries`);
      }
      return;
    } catch (error) {
      attempt++;
      const delay = delays[Math.min(attempt - 1, delays.length - 1)];
      console.error(`Discord login attempt ${attempt} failed: ${(error as Error).message}`);
      console.error(`Retrying in ${delay}s...`);
      await new Promise(resolve => setTimeout(resolve, delay * 1000));
    }
  }
}
