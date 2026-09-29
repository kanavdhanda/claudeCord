import crypto from "node:crypto";

/**
 * A workspace join token: one line of text you can paste into another Claude chat.
 * It contains the bot token, so treat it like a password.
 */
export interface WorkspaceToken {
  v: 1;
  /** Bot token shared by every device in the workspace */
  botToken: string;
  guildId: string;
  /** The shared channel where the workspace's agents talk */
  channelId: string;
  name: string;
  /** Admin user ids (approve plans, slash commands) */
  admins: string[];
  access: "allowlist" | "anyone";
  anyoneApprove: boolean;
}

const PREFIX = "ccw1";

const sum = (payload: string) => crypto.createHash("sha256").update(payload).digest("hex").slice(0, 6);

export function encodeToken(t: WorkspaceToken): string {
  const payload = Buffer.from(JSON.stringify(t), "utf-8").toString("base64url");
  return `${PREFIX}.${payload}.${sum(payload)}`;
}

export function decodeToken(text: string): WorkspaceToken {
  const raw = text.trim().replace(/\s+/g, "");
  const parts = raw.split(".");
  if (parts.length !== 3 || parts[0] !== PREFIX) {
    throw new Error("That doesn't look like a claudeCord workspace token (it should start with ccw1.).");
  }
  const [, payload, check] = parts;
  if (sum(payload) !== check) {
    throw new Error("The token is damaged (checksum mismatch). Copy the whole line again, including the end.");
  }
  let data: Partial<WorkspaceToken>;
  try {
    data = JSON.parse(Buffer.from(payload, "base64url").toString("utf-8"));
  } catch {
    throw new Error("The token could not be read.");
  }
  if (
    data.v !== 1 ||
    typeof data.botToken !== "string" || !data.botToken ||
    typeof data.guildId !== "string" || !/^\d+$/.test(data.guildId) ||
    typeof data.channelId !== "string" || !/^\d+$/.test(data.channelId) ||
    typeof data.name !== "string" ||
    !Array.isArray(data.admins) || data.admins.length === 0 || !data.admins.every((a) => typeof a === "string" && /^\d+$/.test(a)) ||
    (data.access !== "allowlist" && data.access !== "anyone") ||
    typeof data.anyoneApprove !== "boolean"
  ) {
    throw new Error("The token is missing information. Create a new one with `claudecord workspace token`.");
  }
  return data as WorkspaceToken;
}
