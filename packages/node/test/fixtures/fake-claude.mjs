#!/usr/bin/env node
// A stand-in for the Claude Code TUI, used by the end-to-end tests. It draws the screens the adapter looks for
// and does what a message tells it to, through the same daemon socket the real MCP tools use.
import { pathToFileURL } from "node:url";

const ipc = await import(pathToFileURL(process.env.FAKE_IPC).href);
const agentId = process.env.CLAUDECORD_AGENT_ID;
const call = (req) => ipc.callDaemon({ agentId, ...req });

const out = (s) => process.stdout.write("\x1b[2J\x1b[H" + s);
const idle = (extra = "") => out(`${extra}\n │ > Try "fix the tests"\n ? for shortcuts\n`);
const busy = (note = "") => out(`\n ✽ Working… (2s · esc to interrupt) ${note}\n ? for shortcuts\n`);

let mode = "trust";
let menu = null;
let buf = "";
let inPaste = false;
const queue = [];
let working = false;

function drawTrust() {
  out(" Do you trust the files in this folder?\n\n ❯ 1. Yes, proceed\n   2. No, exit\n\n Enter to confirm · Esc to cancel\n");
}

function drawMenu() {
  const rows = menu.options.map((o, i) => `${i === menu.cursor ? " ❯" : "  "} ${i + 1}. ${o}`).join("\n");
  out(` ${menu.question}\n\n${rows}\n\n Esc to cancel\n`);
}

async function handle(text) {
  // text looks like "[from | thread: t] body". Strip the prefix.
  const m = text.match(/^\[([^\]|]+?)(?: \| thread: [^\]]+)?\]\s*([\s\S]*)$/);
  const from = m?.[1] ?? "?";
  // A targeted message arrives as "@name command", so drop the leading mention.
  const body = (m?.[2] ?? text).replace(/^(@\S+\s+)+/, "").trim();
  busy();
  await new Promise((r) => setTimeout(r, 1200));
  let cmd;
  if ((cmd = body.match(/^say: (.+)$/s))) await call({ op: "say", text: cmd[1] });
  else if ((cmd = body.match(/^env: (\S+)$/))) await call({ op: "say", text: `env ${cmd[1]} -> ${process.env[cmd[1]] ?? "unset"}` });
  else if ((cmd = body.match(/^ask: (.+)$/s))) {
    const r = await call({ op: "ask", question: cmd[1] });
    await call({ op: "say", text: `answer=${r.data}` });
  } else if (body.startsWith("perm:")) {
    menu = { question: "Do you want to proceed?", options: ["Yes", "Yes, and don't ask again", "No, and tell Claude what to do differently"], cursor: 0 };
    mode = "menu";
    drawMenu();
    return;
  } else if ((cmd = body.match(/^assign: (\S+) (.+)$/s))) await call({ op: "assign", to: cmd[1], task: cmd[2] });
  else if ((cmd = body.match(/^Task (T\d+):/))) await call({ op: "taskdone", taskId: cmd[1], summary: `finished ${cmd[1]}` });
  else if ((cmd = body.match(/^sendfile: (\S+) to (\S+)$/))) {
    const r = await call({ op: "send", path: cmd[1], to: cmd[2] });
    await call({ op: "say", text: `sent=${r.ok}` });
  } else if (body.startsWith("sendfile-out:")) {
    const r = await call({ op: "send", path: body.slice(13).trim() });
    await call({ op: "say", text: `sent=${r.ok} ${r.ok ? "" : r.error}` });
  } else if (from === "system" || body.length) {
    // Briefings and anything else: just take it in.
  }
  mode = "idle";
  idle();
}

async function drain() {
  if (working) return;
  working = true;
  while (queue.length) await handle(queue.shift());
  working = false;
}

function submit(raw) {
  for (const para of raw.split(/\n\n/)) if (para.trim()) queue.push(para.trim());
  void drain();
}

process.stdout.write("\x1b[?2004h"); // the real TUI enables bracketed paste
process.stdin.setRawMode?.(true);
process.stdin.resume();
drawTrust();

process.stdin.on("data", (d) => {
  const s = d.toString();
  if (mode === "trust") {
    if (s.includes("\r")) {
      mode = "idle";
      idle();
    }
    return;
  }
  if (mode === "menu") {
    if (s === "\x1b[B" || s === "\x1bOB") menu.cursor = Math.min(menu.options.length - 1, menu.cursor + 1);
    else if (s === "\x1b[A" || s === "\x1bOA") menu.cursor = Math.max(0, menu.cursor - 1);
    else if (s === "\r") {
      const picked = menu.cursor + 1;
      menu = null;
      mode = "idle";
      idle();
      void call({ op: "say", text: `picked=${picked}` });
      return;
    }
    if (menu) drawMenu();
    return;
  }
  // Bracketed paste: text between the markers is data, and only a bare carriage return submits.
  for (let i = 0; i < s.length; ) {
    if (s.startsWith("\x1b[200~", i)) { inPaste = true; i += 6; continue; }
    if (s.startsWith("\x1b[201~", i)) { inPaste = false; i += 6; continue; }
    const ch = s[i++];
    if (ch === "\r" && !inPaste) {
      const text = buf;
      buf = "";
      submit(text);
    } else buf += ch;
  }
});
