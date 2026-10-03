import { type Adapter, type LaunchCtx, type PromptInfo, detectLimit, detectRateLimit, menuKeys, parseMenu, tail } from "./types.js";

// Screen heuristics are generic and must be verified against the live Codex TUI.
export const codex: Adapter = {
  id: "codex",
  binary: "codex",
  rulesVia: "first-message",

  argv(ctx: LaunchCtx): string[] {
    const a = ["codex", "--no-alt-screen"];
    if (ctx.spec.model) a.push("-m", ctx.spec.model);
    if (ctx.policy === "autonomous") a.push("-s", "workspace-write", "-a", "never");
    else if (ctx.policy === "plan") a.push("-s", "read-only", "-a", "on-request");
    else a.push("-s", "workspace-write", "-a", "on-request");
    return a;
  },

  detect(screen) {
    const t = tail(screen, 8);
    const busy = /(esc to interrupt|working|thinking)/i.test(t);
    const prompt = parseMenu(screen);
    return { busy, ready: !busy && !prompt, prompt, limit: detectLimit(screen) ?? detectRateLimit(screen) };
  },

  startupChoice(p: PromptInfo) {
    return /trust/i.test(`${p.question} ${p.options.join(" ")}`) ? p.options.findIndex((o) => /^yes/i.test(o)) : undefined;
  },

  selectKeys: menuKeys,
  otherKeys: () => undefined,
};
