import fs from "node:fs";
import path from "node:path";
import { AttachmentBuilder } from "discord.js";
import type { OutChannel } from "../agents/out-channel.js";
import { createSdkMcpServer, tool } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";
import { getConfig } from "../utils/config.js";
import { redactSecrets } from "../security/redact.js";
import { sha256File, splitFile } from "../files/chunks.js";
import { isOutsideProject } from "../security/dangerous.js";
import type { PeerInfo } from "./context.js";

const BLOCKED_FILE = /(^|[\\/])(\.env(\..*)?|id_rsa.*|id_ed25519.*|.*\.pem|.*\.key|data\.db(-wal|-shm)?|\.bot\.lock)$/i;

export interface BridgeContext {
  channel: OutChannel;
  projectPath: string;
  botName: string;
  peers: PeerInfo[];
  secrets: string[];
}

const text = (t: string, isError = false) => ({ content: [{ type: "text" as const, text: t }], isError });

/** In-process MCP server giving Claude Discord-aware tools for one turn. */
export function createBridgeServer(ctx: BridgeContext) {
  const config = getConfig();

  const sendFile = tool(
    "send_file",
    "Upload a file from the working directory to this Discord channel so the human (or a peer agent) can download it. Files over the size limit are split into numbered parts and reassembled automatically by the receiving bot.",
    { path: z.string().describe("Path to the file, relative to the working directory"), note: z.string().optional().describe("Short caption") },
    async (args) => {
      const abs = path.resolve(ctx.projectPath, args.path);
      if (isOutsideProject(abs, ctx.projectPath)) return text("Refused: file is outside the working directory.", true);
      if (BLOCKED_FILE.test(abs)) return text("Refused: this looks like a secret or bot-internal file.", true);
      if (!fs.existsSync(abs) || !fs.statSync(abs).isFile()) return text(`No such file: ${args.path}`, true);

      let parts;
      try {
        parts = splitFile(abs, Math.floor(config.UPLOAD_LIMIT_MB * 1024 * 1024));
      } catch (e) {
        return text(e instanceof Error ? e.message : String(e), true);
      }
      const sha = parts.length > 1 ? sha256File(abs) : null;
      const caption = redactSecrets(args.note ?? "", ctx.secrets);
      const header = sha ? `${caption}\n📦 ${path.basename(abs)} in ${parts.length} parts — sha256:${sha}`.trim() : caption;

      // Discord allows 10 attachments per message
      for (let i = 0; i < parts.length; i += 10) {
        const batch = parts.slice(i, i + 10).map((p) => new AttachmentBuilder(p.data, { name: p.name }));
        await ctx.channel.send({ content: i === 0 && header ? header : undefined, files: batch });
      }
      return text(`Sent ${path.basename(abs)}${parts.length > 1 ? ` as ${parts.length} parts` : ""}.`);
    },
  );

  const askPeer = tool(
    "ask_peer",
    "Send a message to another agent in this channel. It only acts because you @mention it, and every exchange costs the human's usage, so keep it short.",
    {
      peer: z.string().describe("Name of the peer agent (from the list in your instructions)"),
      message: z.string().max(1800).describe("What you need, self-contained"),
      kind: z.enum(["message", "subagent_task"]).default("message").describe("subagent_task asks the peer to run the job in its own subagent"),
      expect_reply: z.boolean().default(false).describe("True only if you need an answer back"),
    },
    async (args) => {
      const peer = ctx.peers.find((p) => p.name.toLowerCase() === args.peer.toLowerCase());
      if (!peer) return text(`Unknown peer "${args.peer}". Known: ${ctx.peers.map((p) => p.name).join(", ") || "none"}`, true);
      const tags = [args.kind === "subagent_task" ? "[subagent_task]" : "", args.expect_reply ? "[reply-requested]" : ""].filter(Boolean).join(" ");
      const body = redactSecrets(args.message, ctx.secrets);
      await ctx.channel.send({
        content: `<@&${peer.id}> **from ${ctx.botName}** ${tags}\n${body}`,
        allowedMentions: { roles: [peer.id] },
      });
      return text(
        args.expect_reply
          ? `Sent to ${peer.name}. Their answer will arrive as a new message; finish your turn now.`
          : `Sent to ${peer.name}. No reply was requested; continue or finish your turn.`,
      );
    },
  );

  return createSdkMcpServer({ name: "claudecord", version: "1.0.0", tools: ctx.peers.length > 0 ? [sendFile, askPeer] : [sendFile] });
}
