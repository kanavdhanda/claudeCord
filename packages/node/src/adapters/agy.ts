import { type Adapter, type LaunchCtx, type PromptInfo, detectLimit, detectRateLimit, menuKeys, parseMenu, tail } from "./types.js";

// Screen heuristics are generic and must be verified against the live agy TUI.
export const agy: Adapter = {
  id: "agy",
  binary: "agy",
  rulesVia: "first-message",

  argv(ctx: LaunchCtx): string[] {
    const a = ["agy"];
    if (ctx.spec.model) a.push("--model", ctx.spec.model);
    if (ctx.policy === "autonomous") a.push("--dangerously-skip-permissions");
    else if (ctx.policy === "plan") a.push("--mode", "plan");
    return a;
  },

  detect(screen) {
    const t = tail(screen, 8);
    const busy = /(esc to (interrupt|cancel)|working|thinking)/i.test(t);
    const prompt = parseMenu(screen);
    return { busy, ready: !busy && !prompt, prompt, limit: detectLimit(screen) ?? detectRateLimit(screen) };
  },

  startupChoice(p: PromptInfo) {
    return /trust/i.test(`${p.question} ${p.options.join(" ")}`) ? p.options.findIndex((o) => /^yes/i.test(o)) : undefined;
  },

  selectKeys: menuKeys,
  otherKeys: () => undefined,
};
