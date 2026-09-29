import { z } from "zod";

export const AgentStatus = z.enum([
  "starting",
  "idle",
  "thinking",
  "executing",
  "waiting_input",
  "paused",
  "limited",
  "offline",
]);
export type AgentStatus = z.infer<typeof AgentStatus>;

export const AdapterId = z.enum(["claude", "agy", "codex"]);
export type AdapterId = z.infer<typeof AdapterId>;

export const AgentSpec = z.object({
  agentId: z.string(),
  name: z.string(),
  project: z.string(),
  adapter: AdapterId,
  model: z.string().optional(),
  role: z.string().optional(),
});
export type AgentSpec = z.infer<typeof AgentSpec>;

// Node -> Hub

export const NodeFrame = z.discriminatedUnion("t", [
  z.object({
    t: z.literal("hello"),
    nodeName: z.string(),
    version: z.string(),
  }),
  z.object({ t: z.literal("agent.register"), agent: AgentSpec, cwd: z.string() }),
  z.object({
    t: z.literal("agent.status"),
    agentId: z.string(),
    status: AgentStatus,
    detail: z.string().optional(),
  }),
  z.object({
    t: z.literal("agent.say"),
    agentId: z.string(),
    text: z.string(),
    thread: z.string().optional(),
  }),
  z.object({
    t: z.literal("agent.ask"),
    agentId: z.string(),
    askId: z.string(),
    question: z.string(),
    options: z.array(z.string()).optional(),
    thread: z.string().optional(),
  }),
  z.object({
    t: z.literal("agent.report"),
    agentId: z.string(),
    title: z.string(),
    summary: z.string(),
    artifacts: z.array(z.string()).optional(),
  }),
  z.object({
    t: z.literal("agent.limit"),
    agentId: z.string(),
    kind: z.enum(["session", "weekly", "rate"]),
    resetsAt: z.string().optional(),
  }),
  z.object({ t: z.literal("agent.gone"), agentId: z.string() }),
  z.object({
    t: z.literal("file.chunk"),
    transferId: z.string(),
    agentId: z.string(),
    name: z.string(),
    seq: z.number().int().nonnegative(),
    last: z.boolean(),
    data: z.string(),
    /** Peer agent name to deliver to. Omitted means post to the Discord channel. */
    to: z.string().optional(),
    caption: z.string().optional(),
    thread: z.string().optional(),
  }),
]);
export type NodeFrame = z.infer<typeof NodeFrame>;

// Hub -> Node

export const HubFrame = z.discriminatedUnion("t", [
  z.object({ t: z.literal("welcome"), nodeId: z.string() }),
  z.object({ t: z.literal("error"), message: z.string() }),
  z.object({
    t: z.literal("deliver"),
    agentId: z.string(),
    from: z.string(),
    text: z.string(),
    thread: z.string().optional(),
  }),
  z.object({
    t: z.literal("answer"),
    agentId: z.string(),
    askId: z.string(),
    text: z.string(),
  }),
  z.object({
    t: z.literal("file.chunk"),
    transferId: z.string(),
    /** Recipient agent. */
    agentId: z.string(),
    from: z.string(),
    name: z.string(),
    seq: z.number().int().nonnegative(),
    last: z.boolean(),
    data: z.string(),
    caption: z.string().optional(),
    thread: z.string().optional(),
  }),
  z.object({ t: z.literal("spawn"), agent: AgentSpec, cwd: z.string().optional() }),
  z.object({ t: z.literal("stop"), agentId: z.string() }),
  z.object({ t: z.literal("killall"), project: z.string().optional() }),
  z.object({
    t: z.literal("hold"),
    on: z.boolean(),
    agentId: z.string().optional(),
    project: z.string().optional(),
  }),
]);
export type HubFrame = z.infer<typeof HubFrame>;

/** Raw bytes per chunk. Base64 on the wire makes each frame about a third larger. */
export const FILE_CHUNK_BYTES = 192 * 1024;
export const MAX_FILE_BYTES = 10 * 1024 * 1024;

export const NODE_CONNECT_PATH = "/api/v1/node/connect";

export function encode(frame: NodeFrame | HubFrame): string {
  return JSON.stringify(frame);
}

export function parseNodeFrame(raw: string): NodeFrame | null {
  try {
    const r = NodeFrame.safeParse(JSON.parse(raw));
    return r.success ? r.data : null;
  } catch {
    return null;
  }
}

export function parseHubFrame(raw: string): HubFrame | null {
  try {
    const r = HubFrame.safeParse(JSON.parse(raw));
    return r.success ? r.data : null;
  } catch {
    return null;
  }
}

const ADJECTIVES = ["quick", "calm", "bright", "steady", "keen", "bold", "wry", "sharp"];
const NOUNS = ["otter", "heron", "lynx", "falcon", "badger", "finch", "marten", "ibis"];

export function autoName(taken: Set<string> = new Set()): string {
  for (let i = 0; i < 50; i++) {
    const n = `${pick(ADJECTIVES)}-${pick(NOUNS)}`;
    if (!taken.has(n)) return n;
  }
  return `agent-${Math.random().toString(36).slice(2, 6)}`;
}

function pick<T>(a: T[]): T {
  return a[Math.floor(Math.random() * a.length)] as T;
}
