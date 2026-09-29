import { describe, it, expect, beforeEach } from "vitest";
import { decidePeerMessage, getHops, recordPeerTurn, resetHops } from "./policy.js";

const base = {
  authorId: "peer1",
  selfId: "me",
  peerIds: ["peer1", "peer2"],
  mentionedIds: ["me"],
  muted: false,
  hops: 0,
  maxHops: 4,
};

describe("decidePeerMessage", () => {
  it("accepts a mentioned message from a known peer", () => {
    expect(decidePeerMessage(base)).toEqual({ accept: true });
  });
  it("ignores unknown bots", () => {
    expect(decidePeerMessage({ ...base, authorId: "stranger" })).toEqual({ accept: false, reason: "not-peer" });
  });
  it("ignores its own messages", () => {
    expect(decidePeerMessage({ ...base, authorId: "me", peerIds: ["me"] })).toEqual({ accept: false, reason: "self" });
  });
  it("requires an explicit mention", () => {
    expect(decidePeerMessage({ ...base, mentionedIds: [] })).toEqual({ accept: false, reason: "not-mentioned" });
  });
  it("respects mute", () => {
    expect(decidePeerMessage({ ...base, muted: true })).toEqual({ accept: false, reason: "muted" });
  });
  it("stops at the hop limit", () => {
    expect(decidePeerMessage({ ...base, hops: 4 })).toEqual({ accept: false, reason: "hop-limit" });
    expect(decidePeerMessage({ ...base, hops: 3 })).toEqual({ accept: true });
  });
});

describe("hop counter", () => {
  beforeEach(() => resetHops("c"));
  it("counts peer turns and resets when a human speaks", () => {
    expect(getHops("c")).toBe(0);
    recordPeerTurn("c");
    expect(recordPeerTurn("c")).toBe(2);
    resetHops("c");
    expect(getHops("c")).toBe(0);
  });
});
