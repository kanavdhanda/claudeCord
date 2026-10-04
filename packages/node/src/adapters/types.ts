import type { AgentSpec } from "@claudecord/protocol";
import type { Policy } from "../config.js";

export interface PromptInfo {
  /** Stable text used to tell whether the same prompt is still showing. */
  signature: string;
  question: string;
  options: string[];
  /** Index of the option the cursor is currently on. */
  cursor: number;
}

export interface LimitInfo {
  kind: "session" | "weekly" | "rate";
  resetsAt?: string;
}

export interface ScreenState {
  busy: boolean;
  ready: boolean;
  executing?: boolean;
  prompt?: PromptInfo;
  limit?: LimitInfo;
}

export interface LaunchCtx {
  spec: AgentSpec;
  policy: Policy;
  rules: string;
  /** Path of an MCP config file that exposes the claudecord tools, if the adapter uses one. */
  mcpConfigPath?: string;
}

export type RulesDelivery = "flag" | "first-message";

export interface Adapter {
  id: string;
  binary: string;
  /** How the team rules reach the agent: a CLI flag, or as the first message once the TUI is ready. */
  rulesVia: RulesDelivery;
  argv(ctx: LaunchCtx): string[];
  detect(screen: string): ScreenState;
  /** Option index to pick automatically for startup dialogs such as folder trust. */
  startupChoice(p: PromptInfo): number | undefined;
  /** tmux keys that move to and confirm option `index`. */
  selectKeys(p: PromptInfo, index: number): string[];
  /** tmux keys that open a free text entry, used when the answer matches no option. */
  otherKeys(p: PromptInfo): string[] | undefined;
}

export function tail(screen: string, n: number): string {
  const lines = screen.split("\n").map((l) => l.trimEnd());
  while (lines.length && !lines[lines.length - 1]) lines.pop();
  return lines.slice(-n).join("\n");
}

/** Shared parser for numbered menus: "1. Yes", "> 2. No", with an optional cursor marker. */
export function parseMenu(screen: string): PromptInfo | undefined {
  const lines = tail(screen, 30).split("\n");
  const opts: { n: number; text: string; cur: boolean; line: number }[] = [];
  lines.forEach((l, line) => {
    const m = l.match(/^\s*([❯>›]\s*)?(\d{1,2})[.)]\s+(.+?)\s*$/);
    if (m) opts.push({ n: Number(m[2]), text: m[3]!, cur: !!m[1], line });
  });
  // Take the last run of consecutively numbered options starting at 1.
  let start = -1;
  for (let i = opts.length - 1; i >= 0; i--)
    if (opts[i]!.n === 1) {
      start = i;
      break;
    }
  if (start < 0) return undefined;
  const run: typeof opts = [];
  for (let i = start; i < opts.length; i++) {
    if (opts[i]!.n === run.length + 1) run.push(opts[i]!);
    else break;
  }
  if (run.length < 2 || !run.some((o) => o.cur)) return undefined;
  const first = run[0]!.line;
  let question = "";
  for (let i = first - 1; i >= 0 && i >= first - 8; i--) {
    const t = lines[i]!.replace(/[│╭╮╰╯─]/g, "").trim();
    if (t) {
      question = t;
      break;
    }
  }
  const options = run.map((o) => o.text);
  return {
    signature: `${question}|${options.join("|")}`,
    question,
    options,
    cursor: Math.max(
      0,
      run.findIndex((o) => o.cur),
    ),
  };
}

export function menuKeys(p: PromptInfo, index: number): string[] {
  const delta = index - p.cursor;
  const keys = Array<string>(Math.abs(delta)).fill(delta >= 0 ? "Down" : "Up");
  return [...keys, "Enter"];
}

const LIMIT_RE =
  /(?:you['\u2019]ve hit your (session|weekly|usage) limit|(session|5-hour|weekly|usage) limit (?:reached|hit)|limit reached)(?:[^\n]*?resets?\s*(?:at\s*)?([^\n]+))?/i;

/** Looks only at the bottom of the screen so words inside agent output rarely trigger it. */
export function detectLimit(screen: string): LimitInfo | undefined {
  const m = tail(screen, 10).match(LIMIT_RE);
  if (!m) return undefined;
  const word = (m[1] ?? m[2] ?? "").toLowerCase();
  const kind = word === "weekly" ? "weekly" : "session";
  return { kind, resetsAt: m[3]?.trim() };
}

export function detectRateLimit(screen: string): LimitInfo | undefined {
  return /rate[- ]limit(ed)?\b.*(retry|try again|exceeded)/i.test(tail(screen, 6)) ? { kind: "rate" } : undefined;
}
