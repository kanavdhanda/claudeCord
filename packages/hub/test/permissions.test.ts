import { describe, expect, it } from "vitest";
import { PermissionsBitField } from "discord.js";
import { REQUIRED_PERMISSIONS, appIdFromToken, inviteUrl, permissionsInteger } from "../src/permissions.js";

const APP_ID = "123456789012345678";
// Real tokens carry the id as unpadded base64.
const b64 = (id: string) => Buffer.from(id).toString("base64").replace(/=+$/, "");
const token = (id = APP_ID) => `${b64(id)}.${"a".repeat(6)}.${"b".repeat(27)}`;

describe("permissions", () => {
  it("asks for exactly the value published in the docs, and never Administrator", () => {
    expect(permissionsInteger()).toBe("309774625872");
    const bits = new PermissionsBitField(BigInt(permissionsInteger()));
    expect(bits.has("Administrator", false)).toBe(false);
    expect(bits.toArray().sort()).toEqual(Object.keys(REQUIRED_PERMISSIONS).sort());
  });

  it("covers what the bot actually does", () => {
    const names = Object.keys(REQUIRED_PERMISSIONS);
    for (const n of [
      "ManageChannels",
      "ManageWebhooks",
      "SendMessages",
      "SendMessagesInThreads",
      "CreatePublicThreads",
      "AddReactions",
      "AttachFiles",
      "ReadMessageHistory",
      "ManageMessages",
      "ViewChannel",
    ]) {
      expect(names).toContain(n);
    }
    expect(names).toHaveLength(10);
  });

  it("explains every permission in words", () => {
    for (const label of Object.values(REQUIRED_PERMISSIONS)) expect(label.length).toBeGreaterThan(5);
  });
});

describe("appIdFromToken", () => {
  it("reads the application id out of a bot token", () => {
    expect(appIdFromToken(token())).toBe(APP_ID);
    expect(appIdFromToken(token("98765432109876543"))).toBe("98765432109876543");
  });

  it.each([
    "",
    "nope",
    "a.b.c",
    "!!!!!!!!!!!!!!!!!!!!!!!!.x.y",
    `${Buffer.from("not digits at all!!").toString("base64")}.a.b`,
    "x".repeat(70),
  ])("rejects %j", (t) => {
    expect(appIdFromToken(t)).toBeUndefined();
  });
});

describe("inviteUrl", () => {
  it("builds the link that adds the bot with the right scopes and permissions", () => {
    const u = new URL(inviteUrl(APP_ID));
    expect(u.origin + u.pathname).toBe("https://discord.com/oauth2/authorize");
    expect(u.searchParams.get("client_id")).toBe(APP_ID);
    expect(u.searchParams.get("scope")).toBe("bot applications.commands");
    expect(u.searchParams.get("permissions")).toBe("309774625872");
  });
});
