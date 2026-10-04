/**
 * The permissions the bot needs, in one place, so the invite link, the self-check and the docs cannot drift apart.
 * Nothing here is Administrator. These are exactly what channels, webhooks, threads, reactions, files and the pinned
 * status board need.
 */
import { PermissionsBitField } from "discord.js";

export const REQUIRED_PERMISSIONS = {
  ViewChannel: "View Channels",
  ManageChannels: "Manage Channels",
  ManageWebhooks: "Manage Webhooks",
  SendMessages: "Send Messages",
  SendMessagesInThreads: "Send Messages in Threads",
  CreatePublicThreads: "Create Public Threads",
  AddReactions: "Add Reactions",
  AttachFiles: "Attach Files",
  ReadMessageHistory: "Read Message History",
  ManageMessages: "Manage Messages (to pin the status board)",
} as const;

export type RequiredPermission = keyof typeof REQUIRED_PERMISSIONS;

/** The permissions as the number Discord puts in an invite link. */
export function permissionsInteger(): string {
  const names = Object.keys(REQUIRED_PERMISSIONS) as RequiredPermission[];
  return new PermissionsBitField(names.map((n) => PermissionsBitField.Flags[n])).bitfield.toString();
}

/**
 * A bot token starts with the base64 of the bot's user ID, which for a bot is also its application ID. So the invite
 * link can be built from the token alone, with nothing more to copy. Returns undefined for something that is not a token.
 */
export function appIdFromToken(token: string): string | undefined {
  const first = token.split(".")[0] ?? "";
  // Real tokens are unpadded base64, but accept padding too.
  if (!/^[A-Za-z0-9_-]{20,30}={0,2}$/.test(first)) return undefined;
  try {
    const id = Buffer.from(first, "base64").toString("utf8");
    return /^\d{17,20}$/.test(id) ? id : undefined;
  } catch {
    return undefined;
  }
}

/** The link that adds the bot to a server with exactly the permissions above. */
export function inviteUrl(appId: string): string {
  const q = new URLSearchParams({
    client_id: appId,
    scope: "bot applications.commands",
    permissions: permissionsInteger(),
  });
  return `https://discord.com/oauth2/authorize?${q.toString()}`;
}
