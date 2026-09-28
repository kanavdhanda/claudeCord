import { Message, Attachment, ActionRowBuilder, ButtonBuilder, ButtonStyle } from "discord.js";
import { getAgentsInChannel, getDirectoryForChannel, getWebhookRecord, recordAudit, getPrimaryAgent } from "../../db/database.js";
import type { Project } from "../../db/types.js";
import { learnFromMessage, outChannelFor } from "../../agents/persona.js";
import { routeHumanMessage } from "../../agents/identity.js";
import { isTerminalHeld, writeRelay } from "../../agents/terminal.js";
import { getConfig } from "../../utils/config.js";
import { decidePeerMessage, getHops, recordPeerTurn, resetHops } from "../../peers/policy.js";
import { parsePartName, storePart } from "../../files/chunks.js";
import { canMessage, checkRateLimit } from "../../security/guard.js";
import { sessionManager } from "../../claude/session-manager.js";
import fs from "node:fs";
import path from "node:path";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { L } from "../../utils/i18n.js";

const IMAGE_EXTENSIONS = new Set([".png", ".jpg", ".jpeg", ".gif", ".webp"]);

// Dangerous executable extensions that should not be downloaded
const BLOCKED_EXTENSIONS = new Set([
  ".exe", ".bat", ".cmd", ".com", ".msi", ".scr", ".pif",
  ".dll", ".sys", ".drv",
  ".vbs", ".vbe", ".wsf", ".wsh",
]);

const MAX_FILE_SIZE = 25 * 1024 * 1024; // 25MB (Discord free tier limit)

async function downloadAttachment(
  attachment: Attachment,
  projectPath: string,
  messageContent = "",
): Promise<{ filePath: string; isImage: boolean } | { skipped: string } | { note: string; filePath?: string } | null> {
  const ext = path.extname(attachment.name ?? "").toLowerCase();

  // Block dangerous executables
  if (BLOCKED_EXTENSIONS.has(ext)) {
    return { skipped: L(`Blocked: \`${attachment.name}\` (dangerous file type)`, `차단됨: \`${attachment.name}\` (위험한 파일 형식)`) };
  }

  // Skip files that are too large
  if (attachment.size > MAX_FILE_SIZE) {
    const sizeMB = (attachment.size / 1024 / 1024).toFixed(1);
    return { skipped: L(`Skipped: \`${attachment.name}\` (${sizeMB}MB exceeds 25MB limit)`, `건너뜀: \`${attachment.name}\` (${sizeMB}MB, 25MB 제한 초과)`) };
  }

  const uploadDir = path.join(projectPath, ".claude-uploads");
  if (!fs.existsSync(uploadDir)) {
    fs.mkdirSync(uploadDir, { recursive: true });
  }

  // Numbered part of a larger file: collect it, and join once every part is here
  if (attachment.name && parsePartName(attachment.name)) {
    try {
      const response = await fetch(attachment.url);
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const data = Buffer.from(await response.arrayBuffer());
      const sha = messageContent.match(/sha256:([0-9a-fA-F]{64})/)?.[1];
      const stored = storePart(uploadDir, attachment.name, data, sha);
      if (stored?.complete && stored.joinedPath) {
        const verdict = stored.hashOk === false ? " ⚠️ checksum mismatch" : stored.hashOk ? " ✔ checksum ok" : "";
        return { note: `Reassembled \`${path.basename(stored.joinedPath)}\` from ${stored.total} parts${verdict}`, filePath: stored.joinedPath };
      }
      return { note: `Received part ${stored?.received}/${stored?.total} of \`${attachment.name}\`` };
    } catch (e) {
      console.warn(`[download] part ${attachment.name} failed:`, e instanceof Error ? e.message : e);
      return { skipped: `Failed to download: \`${attachment.name}\`` };
    }
  }

  const fileName = `${Date.now()}-${attachment.name}`;
  const filePath = path.join(uploadDir, fileName);

  try {
    const response = await fetch(attachment.url);
    if (!response.ok || !response.body) {
      return { skipped: L(`Failed to download: \`${attachment.name}\``, `다운로드 실패: \`${attachment.name}\``) };
    }

    const fileStream = fs.createWriteStream(filePath);
    await pipeline(Readable.fromWeb(response.body as any), fileStream);
  } catch (e) {
    console.warn(`[download] Failed to download attachment ${attachment.name}:`, e instanceof Error ? e.message : e);
    return { skipped: L(`Failed to download: \`${attachment.name}\``, `다운로드 실패: \`${attachment.name}\``) };
  }

  return { filePath, isImage: IMAGE_EXTENSIONS.has(ext) };
}

async function collectAttachments(message: Message, projectPath: string) {
  const imagePaths: string[] = [];
  const filePaths: string[] = [];
  const notes: string[] = [];
  const skipped: string[] = [];
  for (const [, attachment] of message.attachments) {
    const result = await downloadAttachment(attachment, projectPath, message.content);
    if (!result) continue;
    if ("skipped" in result) skipped.push(result.skipped);
    else if ("note" in result) {
      notes.push(result.note);
      if (result.filePath) filePaths.push(result.filePath);
    } else if (result.isImage) imagePaths.push(result.filePath);
    else filePaths.push(result.filePath);
  }
  return { imagePaths, filePaths, notes, skipped };
}

function attachmentPromptSuffix(a: { imagePaths: string[]; filePaths: string[]; notes: string[] }): string {
  let out = "";
  if (a.imagePaths.length > 0) out += `\n\n[Attached images - use Read tool to view these files]\n${a.imagePaths.join("\n")}`;
  if (a.filePaths.length > 0) out += `\n\n[Attached files - use Read tool to read these files]\n${a.filePaths.join("\n")}`;
  if (a.notes.length > 0) out += `\n\n[File transfer]\n${a.notes.join("\n")}`;
  return out;
}

/** Messages written by agents (webhook personas). Only agents that said hello are trusted. */
async function handlePeerMessage(message: Message): Promise<void> {
  const config = getConfig();
  if (!message.webhookId) return; // ordinary bots are ignored
  const agents = getAgentsInChannel(message.channelId);
  if (agents.length === 0) return;

  const directory = getDirectoryForChannel(message.channelId);
  const senderName = message.author.username;
  const sender = directory.find((d) => d.webhook_id === message.webhookId && d.persona === senderName);
  if (!sender) return; // never said hello: untrusted

  const mentionedRoles = [...message.mentions.roles.keys()];
  const targets = agents.filter((a) => a.role_id && a.persona !== senderName && mentionedRoles.includes(a.role_id));

  // Files from a known peer are saved even when nobody is @mentioned (only one candidate receiver)
  const receivers = targets.length > 0 ? targets : agents.filter((a) => a.persona !== senderName);
  if (message.attachments.size > 0 && (targets.length > 0 || receivers.length === 1)) {
    for (const agent of receivers) await collectAttachments(message, agent.project_path);
    await message.react("📥").catch(() => {});
  }

  for (const agent of targets) {
    const ownWebhook = getWebhookRecord(message.channelId)?.webhook_id ?? "";
    const decision = decidePeerMessage({
      authorId: `${sender.webhook_id}:${sender.persona}`,
      selfId: `${ownWebhook}:${agent.persona}`,
      peerIds: directory.map((d) => `${d.webhook_id}:${d.persona}`),
      mentionedIds: mentionedRoles,
      selfMentionId: agent.role_id ?? undefined,
      muted: agent.mute_peers === 1,
      hops: getHops(message.channelId),
      maxHops: config.MAX_PEER_HOPS,
    });
    const out = await outChannelFor(message.client, agent.channel_id);
    if (!out) continue;
    if (!decision.accept) {
      if (decision.reason === "hop-limit") {
        await out.send(
          L(
            `⛔ Agent-to-agent limit (${config.MAX_PEER_HOPS} turns) reached. Send a message to let the agents continue.`,
            `⛔ 에이전트 간 대화 한도(${config.MAX_PEER_HOPS}턴)에 도달했습니다. 메시지를 보내면 계속됩니다.`,
          ),
        );
      }
      continue;
    }
    if (isTerminalHeld(agent)) {
      const body = message.content.replace(/<@&\d+>/g, "").trim();
      writeRelay(agent.persona ?? agent.channel_id, body);
      await out.send(L("→ sent to terminal", "→ 터미널로 전달했습니다."));
      continue;
    }
    if (sessionManager.isActive(agent.channel_id)) {
      await out.send(L("⏳ Busy with another task; the request from the other agent was not queued. Ask again when I'm done.", "⏳ 다른 작업 중입니다. 완료 후 다시 요청해 주세요."));
      continue;
    }

    recordPeerTurn(message.channelId);
    const attachments = { imagePaths: [], filePaths: [], notes: [] as string[] };
    const body = message.content.replace(/<@&\d+>/g, "").trim();
    const wantsReply = body.includes("[reply-requested]");
    recordAudit({ guildId: agent.guild_id, channelId: agent.channel_id, userId: sender.webhook_id, action: "peer-request", detail: `${sender.persona}: ${body.slice(0, 180)}` });
    void sessionManager
      .sendMessage(out, `[Message from peer agent ${sender.persona}]\n${body}${attachmentPromptSuffix(attachments)}`, {
        userId: sender.webhook_id,
        userTag: sender.persona,
        source: "peer",
        peerName: sender.persona,
        replyToPeerRoleId: wantsReply ? sender.role_id : undefined,
      })
      .catch((e) => console.error("peer sendMessage error:", e));
  }
}

export async function handleMessage(message: Message): Promise<void> {
  if (!message.guild) return;
  if (message.author.bot) {
    if (learnFromMessage(message)) return; // a hello: remember who is here
    await handlePeerMessage(message);
    return;
  }

  const agents = getAgentsInChannel(message.channelId);
  if (agents.length === 0) return;

  // An answer to a pending "type your own answer" question goes to whoever asked it
  for (const agent of agents) {
    if (sessionManager.hasPendingCustomInput(agent.channel_id)) {
      const text = message.content.trim();
      if (text && canMessage(message.author.id)) {
        sessionManager.resolveCustomInput(agent.channel_id, text);
        await message.react("✅");
      }
      return;
    }
  }

  const known = new Set(getDirectoryForChannel(message.channelId).map((d) => `${d.webhook_id}:${d.persona}`));
  const targets = routeHumanMessage({
    localAgents: agents.map((a) => ({ key: a.channel_id, persona: a.persona ?? "", roleId: a.role_id })),
    totalKnownAgentsInChannel: Math.max(known.size, agents.length),
    mentionedRoleIds: [...message.mentions.roles.keys()],
    content: message.content,
  });
  // If no agent was @mentioned, fall back to the designated primary agent for this guild
  let resolvedAgents: typeof agents = [];
  if (targets.length > 0) {
    resolvedAgents = targets.map((t) => agents.find((a) => a.channel_id === t.key)!).filter(Boolean);
  } else {
    const primary = getPrimaryAgent(message.guild.id);
    if (primary) resolvedAgents = [primary];
  }
  if (resolvedAgents.length === 0) return;

  // Auth and rate limit apply only to messages that actually address an agent
  if (!canMessage(message.author.id)) {
    await message.reply(L("You are not authorized to use this bot.", "이 봇을 사용할 권한이 없습니다."));
    return;
  }
  if (!checkRateLimit(message.author.id)) {
    await message.reply(L("Rate limit exceeded. Please wait a moment.", "요청 한도를 초과했습니다. 잠시 후 다시 시도하세요."));
    return;
  }

  // A human speaking gives agents a fresh allowance of peer turns
  resetHops(message.channelId);

  for (const agent of resolvedAgents) {
    await runForAgent(message, agent);
  }
}

async function runForAgent(message: Message, project: Project): Promise<void> {
  const key = project.channel_id;

  const out = await outChannelFor(message.client, key);
  if (!out) return;

  if (isTerminalHeld(project)) {
    let prompt = message.content.replace(/<@&\d+>/g, "").replace(/^\s*@\S+(?: \S+)?[:,]?\s*/, (m) => (project.persona && m.toLowerCase().includes(project.persona.toLowerCase()) ? "" : m)).trim();
    const attachments = await collectAttachments(message, project.project_path);
    if (attachments.skipped.length > 0) await message.reply(attachments.skipped.join("\n"));
    prompt += attachmentPromptSuffix(attachments);
    writeRelay(project.persona ?? project.channel_id, prompt);
    await message.reply(L("→ sent to terminal", "→ 터미널로 전달했습니다."));
    return;
  }

  // Drop the address so Claude sees just the request
  let prompt = message.content.replace(/<@&\d+>/g, "").replace(/^@all\s*/i, "").replace(/^\s*@\S+(?: \S+)?[:,]?\s*/, (m) => (project.persona && m.toLowerCase().includes(project.persona.toLowerCase()) ? "" : m)).trim();

  // Download attachments (images, documents, code files, etc.)
  const attachments = await collectAttachments(message, project.project_path);
  if (attachments.skipped.length > 0) {
    await message.reply(attachments.skipped.join("\n"));
  }
  prompt += attachmentPromptSuffix(attachments);

  if (!prompt.trim()) return;

  // If session is active, offer to queue the message
  if (sessionManager.isActive(key)) {
    if (sessionManager.hasQueue(key)) {
      await message.reply(L("⏳ A message is already waiting to be queued. Please press the button first.", "⏳ 이미 큐 추가 대기 중인 메시지가 있습니다. 버튼을 먼저 눌러주세요."));
      return;
    }
    if (sessionManager.isQueueFull(key)) {
      await message.reply(L("⏳ Queue is full (max 5). Please wait for the current task to finish.", "⏳ 큐가 가득 찼습니다 (최대 5개). 현재 작업 완료를 기다려주세요."));
      return;
    }

    sessionManager.setPendingQueue(key, out, prompt, {
      userId: message.author.id,
      userTag: message.author.tag,
      source: "human",
    });

    const row = new ActionRowBuilder<ButtonBuilder>().addComponents(
      new ButtonBuilder().setCustomId(`queue-yes:${key}`).setLabel(L("Add to Queue", "큐에 추가")).setStyle(ButtonStyle.Success).setEmoji("✅"),
      new ButtonBuilder().setCustomId(`queue-no:${key}`).setLabel(L("Cancel", "취소")).setStyle(ButtonStyle.Secondary).setEmoji("❌"),
    );

    await message.reply({
      content: L("⏳ A previous task is in progress. Process this automatically when done?", "⏳ 이전 작업이 진행 중입니다. 완료 후 자동으로 처리할까요?"),
      components: [row],
    });
    return;
  }

  // Send message to Claude session
  await sessionManager.sendMessage(out, prompt, {
    userId: message.author.id,
    userTag: message.author.tag,
    source: "human",
  });
}
