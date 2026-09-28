// Pure helpers for personas: names, keys, routing and the "hello" peers use to find each other.

export function slugify(name: string): string {
  return name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 32) || "agent";
}

/** Webhook display names: 1-80 chars and must not contain Discord's reserved words. */
export function safePersonaName(name: string): string {
  const cleaned = name.replace(/\s+/g, " ").trim().replace(/clyde|discord/gi, "").trim().slice(0, 80);
  return cleaned || "claude";
}

/**
 * The first agent in a channel keeps the plain channel id as its key, so existing
 * single-agent channels keep working. Extra personas get `channelId~slug`.
 */
export function agentKeyFor(discordChannelId: string, persona: string, existingKeysInChannel: string[]): string {
  return existingKeysInChannel.length === 0 ? discordChannelId : `${discordChannelId}~${slugify(persona)}`;
}

export function discordChannelOf(key: string): string {
  return key.split("~")[0];
}

export interface RoutableAgent {
  key: string;
  persona: string;
  roleId: string | null;
}

/**
 * Which local agents should act on a human message?
 * - An @role mention (or a leading `@persona`) addresses that agent.
 * - Without an address, an agent answers only if it is the ONLY agent known in the channel,
 *   so a channel with several agents never wakes all of them at once.
 */
export function routeHumanMessage(input: {
  localAgents: RoutableAgent[];
  totalKnownAgentsInChannel: number;
  mentionedRoleIds: string[];
  content: string;
}): RoutableAgent[] {
  if (/^@all\b/i.test(input.content.trim())) return input.localAgents;

  const byRole = input.localAgents.filter((a) => a.roleId && input.mentionedRoleIds.includes(a.roleId));
  if (byRole.length > 0) return byRole;

  const lead = input.content.trim().match(/^@([^\s:,]+(?: [^\s:,]+)?)/);
  if (lead) {
    const wanted = lead[1].toLowerCase();
    const byName = input.localAgents.filter((a) => wanted === a.persona.toLowerCase() || wanted.startsWith(a.persona.toLowerCase()));
    if (byName.length > 0) return byName;
  }

  const anyoneAddressed = input.mentionedRoleIds.length > 0;
  if (!anyoneAddressed && input.totalKnownAgentsInChannel <= 1 && input.localAgents.length === 1) return input.localAgents;
  return [];
}

// --- hello ---------------------------------------------------------------

export interface Hello {
  persona: string;
  roleId: string;
  webhookId: string;
  host: string;
}

const HELLO_PREFIX = "claudecord:v1";

export function formatHelloFooter(h: Hello): string {
  return [HELLO_PREFIX, `persona=${encodeURIComponent(h.persona)}`, `role=${h.roleId}`, `webhook=${h.webhookId}`, `host=${encodeURIComponent(h.host)}`].join("|");
}

export function parseHelloFooter(text: string | null | undefined): Hello | null {
  if (!text || !text.startsWith(HELLO_PREFIX)) return null;
  const fields = new Map(
    text
      .split("|")
      .slice(1)
      .map((kv) => {
        const i = kv.indexOf("=");
        return [kv.slice(0, i), kv.slice(i + 1)] as const;
      }),
  );
  const persona = fields.get("persona");
  const roleId = fields.get("role");
  const webhookId = fields.get("webhook");
  const host = fields.get("host");
  if (!persona || !roleId || !webhookId || !host) return null;
  if (!/^\d+$/.test(roleId) || !/^\d+$/.test(webhookId)) return null;
  try {
    return { persona: decodeURIComponent(persona), roleId, webhookId, host: decodeURIComponent(host) };
  } catch {
    return null;
  }
}

/** A terminal hold is valid only while the process that set it is alive and recent. */
export function holdIsLive(hold: { pid: number | null; since: number } | null, isAlive: (pid: number) => boolean, now = Date.now(), maxAgeMs = 12 * 3600_000): boolean {
  if (!hold || hold.pid === null) return false;
  if (now - hold.since > maxAgeMs) return false;
  return isAlive(hold.pid);
}
