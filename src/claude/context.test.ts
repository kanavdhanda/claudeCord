import { describe, it, expect } from "vitest";
import { buildContextPrompt, type ContextInput } from "./context.js";

const base: ContextInput = {
  host: "work-mac",
  platform: "darwin",
  projectPath: "/p/app",
  repoUrl: "https://github.com/a/b.git",
  branch: "main",
  channelName: "work-mac-app",
  guildName: "My Server",
  botName: "WorkMac",
  peers: [{ id: "2", name: "HomePC" }],
  planFirst: true,
  maxHops: 4,
  triggeredBy: { kind: "human", userTag: "kd" },
};

describe("buildContextPrompt", () => {
  it("states host, cwd, repo, channel and trigger", () => {
    const t = buildContextPrompt(base);
    expect(t).toContain('"work-mac"');
    expect(t).toContain("/p/app");
    expect(t).toContain("github.com/a/b.git (branch main)");
    expect(t).toContain("#work-mac-app");
    expect(t).toContain("human kd");
  });
  it("describes peers, the hop cap and subagent tasks", () => {
    const t = buildContextPrompt(base);
    expect(t).toContain("HomePC");
    expect(t).toContain("capped at 4");
    expect(t).toContain("subagent_task");
  });
  it("handles no peers and peer-triggered turns", () => {
    const t = buildContextPrompt({ ...base, peers: [], triggeredBy: { kind: "peer", peerName: "HomePC" } });
    expect(t).toContain("only agent here");
    expect(t).toContain('peer agent "HomePC"');
    expect(t).not.toContain("ask_peer");
  });
  it("omits the plan rule when plan-first is off", () => {
    expect(buildContextPrompt({ ...base, planFirst: false })).not.toContain("approval before making changes");
  });
});
