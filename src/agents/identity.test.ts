import { describe, it, expect } from "vitest";
import { agentKeyFor, discordChannelOf, formatHelloFooter, holdIsLive, parseHelloFooter, routeHumanMessage, safePersonaName, slugify } from "./identity.js";

const a = { key: "c1", persona: "api", roleId: "R1" };
const b = { key: "c1~docs", persona: "docs", roleId: "R2" };

describe("identity", () => {
  it("slugifies and sanitises names", () => {
    expect(slugify("My Agent!")).toBe("my-agent");
    expect(slugify("!!!")).toBe("agent");
    expect(safePersonaName("Discord Clyde bot")).toBe("bot");
    expect(safePersonaName("   ")).toBe("claude");
  });
  it("keeps the plain channel id for the first agent only", () => {
    expect(agentKeyFor("c1", "api", [])).toBe("c1");
    expect(agentKeyFor("c1", "Docs Bot", ["c1"])).toBe("c1~docs-bot");
    expect(discordChannelOf("c1~docs-bot")).toBe("c1");
    expect(discordChannelOf("c1")).toBe("c1");
  });
});

describe("routeHumanMessage", () => {
  it("routes by role mention", () => {
    expect(routeHumanMessage({ localAgents: [a, b], totalKnownAgentsInChannel: 2, mentionedRoleIds: ["R2"], content: "hi" })).toEqual([b]);
  });
  it("routes by leading @name when roles are unavailable", () => {
    expect(routeHumanMessage({ localAgents: [a, b], totalKnownAgentsInChannel: 2, mentionedRoleIds: [], content: "@Docs please fix" })).toEqual([b]);
  });
  it("answers an unaddressed message only when it is the only agent", () => {
    expect(routeHumanMessage({ localAgents: [a], totalKnownAgentsInChannel: 1, mentionedRoleIds: [], content: "do it" })).toEqual([a]);
    expect(routeHumanMessage({ localAgents: [a], totalKnownAgentsInChannel: 2, mentionedRoleIds: [], content: "do it" })).toEqual([]);
    expect(routeHumanMessage({ localAgents: [a, b], totalKnownAgentsInChannel: 2, mentionedRoleIds: [], content: "do it" })).toEqual([]);
  });
  it("stays quiet when someone else's role is mentioned", () => {
    expect(routeHumanMessage({ localAgents: [a], totalKnownAgentsInChannel: 1, mentionedRoleIds: ["OTHER"], content: "hey" })).toEqual([]);
  });
});

describe("hello footer", () => {
  it("round-trips including awkward names", () => {
    const h = { persona: "api | agent=1", roleId: "123", webhookId: "456", host: "work mac" };
    expect(parseHelloFooter(formatHelloFooter(h))).toEqual(h);
  });
  it("rejects anything else", () => {
    expect(parseHelloFooter("hello")).toBeNull();
    expect(parseHelloFooter("claudecord:v1|persona=x|role=abc|webhook=1|host=h")).toBeNull();
    expect(parseHelloFooter(null)).toBeNull();
  });
});

describe("holdIsLive", () => {
  const alive = () => true;
  const dead = () => false;
  it("needs a live pid and a recent hold", () => {
    expect(holdIsLive({ pid: 5, since: 1000 }, alive, 2000)).toBe(true);
    expect(holdIsLive({ pid: 5, since: 1000 }, dead, 2000)).toBe(false);
    expect(holdIsLive({ pid: 5, since: 0 }, alive, 13 * 3600_000)).toBe(false);
    expect(holdIsLive({ pid: null, since: 1 }, alive, 2)).toBe(false);
    expect(holdIsLive(null, alive)).toBe(false);
  });
});
