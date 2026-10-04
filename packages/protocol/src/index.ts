import { z } from "zod";

/** Raw bytes per chunk. Base64 on the wire makes each frame about a third larger. */
export const FILE_CHUNK_BYTES = 192 * 1024;
export const MAX_FILE_BYTES = 10 * 1024 * 1024;

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

/**
 * Names end up in Discord channel names, tmux titles, file paths and prompts, and arrive from machines that may be
 * compromised, so they are validated here once rather than trusted at every use.
 */
export const Slug = z.string().regex(/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/, "use letters, digits, dot, dash or underscore, up to 64 characters");
const Model = z.string().regex(/^[A-Za-z0-9][A-Za-z0-9._:/@+\-\[\]]{0,79}$/, "invalid model name");
const Role = z.string().max(120).regex(/^[^\x00-\x1f\x7f]*$/, "no control characters");

export const AgentSpec = z
  .object({
    agentId: z.string().max(200),
    name: Slug,
    project: Slug,
    adapter: AdapterId,
    model: Model.optional(),
    role: Role.optional(),
  })
  .refine((a) => a.agentId === `${a.project}/${a.name}`, { message: "agentId must be project/name" });
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
    text: z.string().max(8000),
    thread: z.string().max(90).optional(),
  }),
  z.object({
    t: z.literal("agent.ask"),
    agentId: z.string(),
    askId: z.string().max(300),
    question: z.string().max(4000),
    options: z.array(z.string().max(200)).max(12).optional(),
    thread: z.string().max(90).optional(),
  }),
  z.object({
    t: z.literal("agent.report"),
    agentId: z.string(),
    title: z.string().max(200),
    summary: z.string().max(8000),
    artifacts: z.array(z.string().max(500)).max(20).optional(),
  }),
  z.object({
    t: z.literal("agent.limit"),
    agentId: z.string(),
    kind: z.enum(["session", "weekly", "rate"]),
    resetsAt: z.string().optional(),
  }),
  z.object({ t: z.literal("agent.gone"), agentId: z.string() }),
  z.object({
    t: z.literal("agent.accepted"),
    agentId: z.string(),
    /** Delivery ids the agent has picked up and started working on. */
    msgIds: z.array(z.string().max(40)).max(200),
  }),
  z.object({
    t: z.literal("agent.assign"),
    agentId: z.string(),
    to: z.string().max(64),
    task: z.string().max(4000),
    thread: z.string().max(90).optional(),
  }),
  z.object({
    t: z.literal("agent.taskdone"),
    agentId: z.string(),
    taskId: z.string().max(20),
    summary: z.string().max(4000),
  }),
  z.object({
    t: z.literal("file.chunk"),
    transferId: z.string().max(80),
    agentId: z.string(),
    name: z.string().max(255),
    seq: z.number().int().nonnegative().max(1000),
    last: z.boolean(),
    // One chunk is at most FILE_CHUNK_BYTES raw, which is a third larger as base64.
    data: z.string().max(Math.ceil((FILE_CHUNK_BYTES * 4) / 3) + 16),
    /** Peer agent name to deliver to. Omitted means post to the Discord channel. */
    to: z.string().max(64).optional(),
    caption: z.string().max(1000).optional(),
    thread: z.string().max(90).optional(),
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
    /** Echoed back in agent.accepted once the agent starts on this delivery. */
    msgId: z.string().optional(),
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
  z.object({ t: z.literal("spawn"), agent: AgentSpec }),
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

export { findSecretsInFile, looksLikeEnvDump, redact, type Redaction } from "./redact.js";
export { ask, isInteractive, type AskOptions } from "./tty.js";
