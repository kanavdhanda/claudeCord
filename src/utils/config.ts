import os from "node:os";
import path from "node:path";
import dotenv from "dotenv";
import { z } from "zod";
import { appRoot } from "./paths.js";

const idList = z
  .string()
  .optional()
  .transform((v) => (v ? v.split(",").map((id) => id.trim()).filter(Boolean) : []));

const envSchema = z.object({
  DISCORD_BOT_TOKEN: z.string().min(1, "DISCORD_BOT_TOKEN is required"),
  // Legacy single-server setting; DISCORD_GUILD_IDS (comma-separated) restricts to several.
  // If neither is set the bot works in every server it has been invited to.
  DISCORD_GUILD_ID: z.string().optional(),
  DISCORD_GUILD_IDS: idList,
  // Name other agents see in the context prompt (defaults to this machine's hostname)
  HOST_NAME: z
    .string()
    .optional()
    .transform((v) => (v && v.length > 0 ? v : os.hostname())),
  // Max consecutive agent-to-agent turns in a channel before a human must step in
  MAX_PEER_HOPS: z.coerce.number().int().positive().default(4),
  // Largest single file posted to Discord; bigger files are split into numbered parts
  UPLOAD_LIMIT_MB: z.coerce.number().positive().default(9),
  // A plan approved this recently covers follow-up messages in the same channel
  PLAN_TTL_MIN: z.coerce.number().positive().default(30),
  // Require an approved plan before Claude starts working on a task
  PLAN_FIRST: z
    .enum(["true", "false"])
    .default("true")
    .transform((v) => v === "true"),
  ALLOWED_USER_IDS: z
    .string()
    .min(1, "ALLOWED_USER_IDS is required")
    .transform((v) => v.split(",").map((id) => id.trim())),
  // Who may talk to the agents: only the allowlist, or anyone in the server.
  // ALLOWED_USER_IDS are always the admins: they approve plans/tools and use slash commands.
  ACCESS_MODE: z.enum(["allowlist", "anyone"]).default("allowlist"),
  // With ACCESS_MODE=anyone, also let anyone approve plans and tool use. Think twice: an
  // approver can make an agent run commands on your machine.
  ANYONE_CAN_APPROVE: z
    .enum(["true", "false"])
    .default("false")
    .transform((v) => v === "true"),
  BASE_PROJECT_DIR: z.string().min(1, "BASE_PROJECT_DIR is required"),
  RATE_LIMIT_PER_MINUTE: z.coerce.number().int().positive().default(10),
  SHOW_COST: z
    .enum(["true", "false"])
    .default("true")
    .transform((v) => v === "true"),
  CLAUDE_MODEL: z
    .string()
    .optional()
    .transform((v) => (v && v.length > 0 ? v : undefined)),
});

export function allowedGuildIds(config: Config): string[] {
  const ids = [...config.DISCORD_GUILD_IDS];
  if (config.DISCORD_GUILD_ID) ids.push(config.DISCORD_GUILD_ID);
  return ids;
}

export type Config = z.infer<typeof envSchema>;

let _config: Config | null = null;

export function loadConfig(): Config {
  if (_config) return _config;

  // The app folder's .env applies no matter where the command is run from (does not override real env vars)
  if (!process.env.VITEST) dotenv.config({ path: path.join(appRoot(), ".env"), quiet: true });
  const result = envSchema.safeParse(process.env);
  if (!result.success) {
    const errors = result.error.issues
      .map((i) => `  - ${i.path.join(".")}: ${i.message}`)
      .join("\n");
    console.error(`Configuration error:\n${errors}`);
    process.exit(1);
  }

  _config = result.data;
  return _config;
}

export function getConfig(): Config {
  if (!_config) return loadConfig();
  return _config;
}
