import {
  type Adapter,
  type LaunchCtx,
  type PromptInfo,
  detectLimit,
  detectRateLimit,
  menuKeys,
  parseMenu,
  tail,
} from "./types.js";

const MENU_HINT = /esc to cancel|enter to select|do you want|do you trust|bypass permissions|select/i;

export const claude: Adapter = {
  id: "claude",
  binary: "claude",
  rulesVia: "flag",

  argv(ctx: LaunchCtx): string[] {
    const a = ["claude"];
    if (ctx.spec.model) a.push("--model", ctx.spec.model);
    if (ctx.policy === "autonomous") a.push("--dangerously-skip-permissions");
    else if (ctx.policy === "plan") a.push("--permission-mode", "plan");
    if (ctx.mcpConfigPath) {
      a.push("--mcp-config", ctx.mcpConfigPath);
      a.push(
        "--allowedTools",
        ...["say", "ask_human", "report", "send_file", "assign", "task_done"].map((t) => `mcp__claudecord__${t}`),
      );
    }
    a.push("--append-system-prompt", ctx.rules, "-n", ctx.spec.name);
    return a;
  },

  detect(screen) {
    const t = tail(screen, 15);
    const busy = /esc to interrupt/i.test(t);
    const menu = parseMenu(screen);
    const prompt = menu && MENU_HINT.test(tail(screen, 30)) ? menu : undefined;
    const ready =
      !busy && !prompt && /(\? for shortcuts|bypass permissions|plan mode|accept edits|auto mode|❯)/i.test(t);
    const executing = busy && /(running|⎿\s+\S+\.{3}|\(\d+s)/i.test(t);
    return { busy, ready, executing, prompt, limit: detectLimit(screen) ?? detectRateLimit(screen) };
  },

  startupChoice(p: PromptInfo) {
    const all = `${p.question} ${p.options.join(" ")}`;
    if (/trust/i.test(all)) return p.options.findIndex((o) => /^yes/i.test(o));
    if (/bypass permissions/i.test(all)) return p.options.findIndex((o) => /yes.*accept/i.test(o));
    return undefined;
  },

  selectKeys: menuKeys,

  otherKeys(p) {
    const i = p.options.findIndex((o) => /^(other|type something|chat about this)/i.test(o));
    return i >= 0 ? menuKeys(p, i) : undefined;
  },
};
