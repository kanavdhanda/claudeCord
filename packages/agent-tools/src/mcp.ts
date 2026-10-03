#!/usr/bin/env node
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { callDaemon, currentAgentId, type AgentRequest } from "./ipc.js";

const server = new McpServer({ name: "claudecord", version: "0.1.0" });

async function send(req: AgentRequest) {
  const r = await callDaemon(req);
  if (!r.ok) return { content: [{ type: "text" as const, text: `error: ${r.error}` }], isError: true };
  const text = typeof r.data === "string" ? r.data : "sent";
  return { content: [{ type: "text" as const, text }] };
}

server.tool(
  "say",
  "Post a short chat message (1-3 sentences) to the team channel. Mention peers as @name. Optionally name a thread for a sub-topic.",
  { message: z.string(), thread: z.string().optional() },
  ({ message, thread }) => send({ op: "say", agentId: currentAgentId(), text: message, thread }),
);

server.tool(
  "ask_human",
  "Ask the engineer (the human) a question when blocked or at a decision point. Blocks until they reply and returns their answer.",
  { question: z.string(), options: z.array(z.string()).optional(), thread: z.string().optional() },
  ({ question, options, thread }) =>
    send({ op: "ask", agentId: currentAgentId(), question, options, thread }),
);

server.tool(
  "report",
  "Post the single final report when the task is complete: what was done and any artifacts such as files or PR links.",
  { title: z.string(), summary: z.string(), artifacts: z.array(z.string()).optional() },
  ({ title, summary, artifacts }) =>
    send({ op: "report", agentId: currentAgentId(), title, summary, artifacts }),
);

await server.connect(new StdioServerTransport());
