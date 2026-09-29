import { describe, expect, it } from "vitest";
import { secretEnvNames, withScrubbedEnv } from "../src/env.js";

const ghp = "ghp_" + "a".repeat(36);

describe("secretEnvNames", () => {
  const env = {
    PATH: "/usr/bin",
    HOME: "/home/u",
    LANG: "en_US.UTF-8",
    AWS_ACCESS_KEY_ID: "AKIA" + "ABCDEFGHIJKLMNOP",
    AWS_SECRET_ACCESS_KEY: "abc123",
    GITHUB_TOKEN: ghp,
    MY_SERVICE_TOKEN: "something",
    DB_PASSWORD: "hunter2",
    STRIPE_API_KEY: "sk_live_xxx",
    DATABASE_URL: "postgres://u:p@h/db",
    NPM_CONFIG_USERCONFIG: "/x/.npmrc",
    INNOCENT_NAME: ghp,
    EDITOR: "vim",
    ANTHROPIC_API_KEY: "sk-ant-" + "x".repeat(40),
    OPENAI_API_KEY: "sk-" + "y".repeat(40),
    CLAUDECORD_AGENT_ID: "p/a",
    CLAUDECORD_SOCK: "/tmp/s",
  };

  it("removes credentials by name, by prefix and by value", () => {
    const out = secretEnvNames(env, "agy");
    for (const n of [
      "AWS_ACCESS_KEY_ID",
      "AWS_SECRET_ACCESS_KEY",
      "GITHUB_TOKEN",
      "MY_SERVICE_TOKEN",
      "DB_PASSWORD",
      "STRIPE_API_KEY",
      "DATABASE_URL",
      "NPM_CONFIG_USERCONFIG",
      "INNOCENT_NAME",
    ]) {
      expect(out, n).toContain(n);
    }
  });

  it("keeps ordinary variables and claudecord's own", () => {
    const out = secretEnvNames(env, "claude");
    for (const n of ["PATH", "HOME", "LANG", "EDITOR", "CLAUDECORD_AGENT_ID", "CLAUDECORD_SOCK"])
      expect(out).not.toContain(n);
  });

  it("keeps only the login each agent needs for its own service", () => {
    expect(secretEnvNames(env, "claude")).not.toContain("ANTHROPIC_API_KEY");
    expect(secretEnvNames(env, "claude")).toContain("OPENAI_API_KEY");
    expect(secretEnvNames(env, "codex")).not.toContain("OPENAI_API_KEY");
    expect(secretEnvNames(env, "codex")).toContain("ANTHROPIC_API_KEY");
  });

  it("keeps the ssh agent socket so git over ssh still works", () => {
    expect(secretEnvNames({ SSH_AUTH_SOCK: "/tmp/ssh-agent.sock", SSH_PASSWORD: "hunter2hunter2" }, "claude")).toEqual([
      "SSH_PASSWORD",
    ]);
  });

  it("ignores variables with no value", () => {
    expect(secretEnvNames({ SOME_TOKEN: undefined }, "claude")).toEqual([]);
  });
});

describe("withScrubbedEnv", () => {
  it("wraps the command with env -u for each name", () => {
    expect(withScrubbedEnv(["claude", "--model", "x"], ["A", "B"])).toEqual([
      "env",
      "-u",
      "A",
      "-u",
      "B",
      "claude",
      "--model",
      "x",
    ]);
  });

  it("leaves the command alone when there is nothing to remove", () => {
    expect(withScrubbedEnv(["claude"], [])).toEqual(["claude"]);
  });
});
