import { describe, it, expect } from "vitest";
import { decodeToken, encodeToken, type WorkspaceToken } from "./token.js";

const t: WorkspaceToken = {
  v: 1,
  botToken: "abc.def.ghi",
  guildId: "123",
  channelId: "456",
  name: "my workspace",
  admins: ["111", "222"],
  access: "anyone",
  anyoneApprove: false,
};

describe("workspace token", () => {
  it("round-trips", () => {
    const enc = encodeToken(t);
    expect(enc.startsWith("ccw1.")).toBe(true);
    expect(decodeToken(enc)).toEqual(t);
  });
  it("tolerates surrounding whitespace and line wraps from copy-paste", () => {
    const enc = encodeToken(t);
    expect(decodeToken(`  ${enc.slice(0, 20)}\n${enc.slice(20)}  `)).toEqual(t);
  });
  it("detects truncation and tampering", () => {
    const enc = encodeToken(t);
    expect(() => decodeToken(enc.slice(0, -3))).toThrow(/damaged|checksum/i);
    const [p, body, c] = enc.split(".");
    expect(() => decodeToken(`${p}.${body.slice(0, -2)}xx.${c}`)).toThrow();
  });
  it("rejects other strings and incomplete data", () => {
    expect(() => decodeToken("hello")).toThrow(/claudeCord workspace token/);
    const bad = encodeToken({ ...t, admins: [] });
    expect(() => decodeToken(bad)).toThrow(/missing information/);
  });
});
