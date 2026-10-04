import { describe, expect, it } from "vitest";
import { Bucket, FailureLimiter } from "../src/limits.js";
import { isDiscordCdn } from "../src/discord.js";

describe("Bucket", () => {
  it("allows a burst, then refuses, then refills over time", () => {
    const t = 1_000_000;
    const b = new Bucket(5, 10, t);
    for (let i = 0; i < 5; i++) expect(b.take(1, t)).toBe(true);
    expect(b.take(1, t)).toBe(false);
    expect(b.take(1, t + 100)).toBe(true); // 100 ms refills one token at 10 per second
    expect(b.take(1, t + 100)).toBe(false);
  });

  it("never holds more than its capacity", () => {
    const t = 5;
    const b = new Bucket(3, 1000, t);
    expect(b.take(1, t + 60_000)).toBe(true);
    expect(b.take(3, t + 60_000)).toBe(false);
  });
});

describe("Bucket clock safety", () => {
  it("is not drained by a clock that goes backwards", () => {
    const b = new Bucket(2, 1, 10_000);
    expect(b.take(1, 5_000)).toBe(true);
    expect(b.take(1, 5_000)).toBe(true);
    expect(b.take(1, 5_000)).toBe(false);
    expect(b.take(1, 12_000)).toBe(true);
  });
});

describe("FailureLimiter", () => {
  it("blocks a key after too many failures and unblocks after the window", () => {
    const f = new FailureLimiter(3, 1000);
    const t = 10_000;
    expect(f.blocked("1.2.3.4", t)).toBe(false);
    for (let i = 0; i < 3; i++) f.fail("1.2.3.4", t);
    expect(f.blocked("1.2.3.4", t)).toBe(true);
    expect(f.blocked("5.6.7.8", t)).toBe(false);
    expect(f.blocked("1.2.3.4", t + 1001)).toBe(false);
  });
});

describe("isDiscordCdn", () => {
  it.each(["https://cdn.discordapp.com/attachments/1/2/f.txt", "https://media.discordapp.net/attachments/1/2/f.png"])("allows %s", (u) => {
    expect(isDiscordCdn(u)).toBe(true);
  });

  it.each([
    "http://cdn.discordapp.com/x",
    "https://evil.example.com/cdn.discordapp.com",
    "https://cdn.discordapp.com.evil.example.com/x",
    "https://169.254.169.254/latest/meta-data",
    "file:///etc/passwd",
    "javascript:alert(1)",
    "not a url",
  ])("refuses %s", (u) => {
    expect(isDiscordCdn(u)).toBe(false);
  });
});
