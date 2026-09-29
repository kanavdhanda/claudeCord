// Rules for when this agent may act on a message written by another bot.
// Kept free of Discord types so it can be unit tested.

const hopCounts = new Map<string, number>();

export function getHops(channelId: string): number {
  return hopCounts.get(channelId) ?? 0;
}

/** Called when a human speaks: agents get a fresh allowance of turns. */
export function resetHops(channelId: string): void {
  hopCounts.delete(channelId);
}

export function recordPeerTurn(channelId: string): number {
  const next = getHops(channelId) + 1;
  hopCounts.set(channelId, next);
  return next;
}

export type PeerDecision =
  | { accept: true }
  | { accept: false; reason: "not-peer" | "not-mentioned" | "muted" | "hop-limit" | "self" };

export function decidePeerMessage(input: {
  /** Sender identity (for personas: webhook id + name) */
  authorId: string;
  /** Own sender identity, so an agent never answers itself */
  selfId: string;
  /** Sender identities of agents this one may listen to */
  peerIds: string[];
  /** What was mentioned in the message (role ids for personas) */
  mentionedIds: string[];
  /** The mention that addresses this agent (its role id) */
  selfMentionId?: string;
  muted: boolean;
  hops: number;
  maxHops: number;
}): PeerDecision {
  if (input.authorId === input.selfId) return { accept: false, reason: "self" };
  if (!input.peerIds.includes(input.authorId)) return { accept: false, reason: "not-peer" };
  // Only an explicit @mention wakes an agent, so idle chatter never costs tokens
  if (!input.mentionedIds.includes(input.selfMentionId ?? input.selfId)) return { accept: false, reason: "not-mentioned" };
  if (input.muted) return { accept: false, reason: "muted" };
  if (input.hops >= input.maxHops) return { accept: false, reason: "hop-limit" };
  return { accept: true };
}
