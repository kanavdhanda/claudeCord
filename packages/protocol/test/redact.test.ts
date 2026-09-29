import { describe, expect, it } from "vitest";
import { findSecretsInFile, looksLikeEnvDump, redact } from "../src/redact.js";

// Built at runtime so this file does not itself look like a leaked credential to scanners.
const fake = {
  aws: "AKIA" + "ABCDEFGHIJKLMNOP",
  github: "ghp_" + "a".repeat(36),
  anthropic: "sk-ant-" + "x".repeat(40),
  slack: "xoxb-" + "1234567890-abcdefghij",
  discord: "M" + "T".repeat(23) + "." + "abcdef" + "." + "z".repeat(27),
  device: "ccn1." + "A".repeat(32),
  google: "AIza" + "B".repeat(35),
  jwt: "eyJ" + "a".repeat(12) + ".eyJ" + "b".repeat(12) + "." + "c".repeat(12),
};

describe("redact", () => {
  it.each(Object.entries(fake))("removes a %s", (kind, secret) => {
    const r = redact(`here it is: ${secret} please use it`);
    expect(r.text).not.toContain(secret);
    expect(r.text).toContain("[redacted");
    expect(r.found.length).toBeGreaterThan(0);
    void kind;
  });

  it("removes a whole private key block", () => {
    const key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
    const r = redact(`my key:\n${key}\nthanks`);
    expect(r.text).not.toContain("b3BlbnNzaC1rZXk");
    expect(r.text).toContain("thanks");
  });

  it("removes a truncated private key too, up to the end of the text", () => {
    expect(redact("-----BEGIN RSA PRIVATE KEY-----\nMIIEow").text).not.toContain("MIIEow");
  });

  it("removes the value of secret-looking assignments but keeps the name", () => {
    const r = redact("export DATABASE_PASSWORD=hunter2hunter2 and API_KEY: 'abcdefgh12345'");
    expect(r.text).toContain("DATABASE_PASSWORD=");
    expect(r.text).toContain("API_KEY");
    expect(r.text).not.toContain("hunter2hunter2");
    expect(r.text).not.toContain("abcdefgh12345");
  });

  it("removes bearer tokens", () => {
    const r = redact("curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456'");
    expect(r.text).not.toContain("abcdefghijklmnopqrstuvwxyz123456");
    expect(r.text).toContain("Bearer");
  });

  it("leaves ordinary text and code alone", () => {
    const text =
      "Plan: heron takes the API (POST /export), I take the UI.\nconst tokenizer = new Tokenizer(); // sk-learn style names\nSee README.md";
    const r = redact(text);
    expect(r.text).toBe(text);
    expect(r.found).toEqual([]);
  });

  it("does not flag short or placeholder values", () => {
    expect(redact("PASSWORD=short").found).toEqual([]);
    expect(redact("TOKEN=").found).toEqual([]);
  });

  it("handles every match in a message, and many, without hanging", () => {
    const text = Array(2000).fill(`${fake.github} and ${fake.aws}`).join("\n");
    const start = Date.now();
    const r = redact(text);
    expect(Date.now() - start).toBeLessThan(2000);
    expect(r.found).toHaveLength(4000);
  });

  it("is safe on long pathological input", () => {
    const start = Date.now();
    redact("A".repeat(200_000) + "=" + " ".repeat(50_000) + "-----BEGIN PRIVATE KEY-----" + "x".repeat(100_000));
    expect(Date.now() - start).toBeLessThan(3000);
  });
});

describe("idempotence", () => {
  const samples = [
    "export DATABASE_PASSWORD=hunter2hunter2 and API_KEY: 'abcdefgh12345'",
    "curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456'",
    `token ${fake.github} and ${fake.aws} and ${fake.jwt}`,
    "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
    "nothing secret here at all",
  ];

  it.each(samples)("redacting twice gives the same text as redacting once: %s", (text) => {
    const once = redact(text);
    const twice = redact(once.text);
    expect(twice.text).toBe(once.text);
    expect(twice.found).toEqual([]);
  });
});

describe("findSecretsInFile", () => {
  it("flags text files that contain high confidence secrets", () => {
    expect(findSecretsInFile(Buffer.from(`config\nkey=${fake.aws}\n`))).toContain("aws key");
    expect(findSecretsInFile(Buffer.from("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"))).toContain(
      "private key",
    );
  });

  it("does not block on weak signals such as a variable named token", () => {
    expect(findSecretsInFile(Buffer.from("const token = getToken(); password = prompt()"))).toEqual([]);
    expect(findSecretsInFile(Buffer.from("API_KEY=placeholder_value_here"))).toEqual([]);
  });

  it("skips binary files", () => {
    expect(findSecretsInFile(Buffer.concat([Buffer.from([0, 1, 2, 3]), Buffer.from(fake.aws)]))).toEqual([]);
  });

  it("can be called repeatedly without state leaking between calls", () => {
    const buf = Buffer.from(fake.github);
    expect(findSecretsInFile(buf)).toEqual(["github token"]);
    expect(findSecretsInFile(buf)).toEqual(["github token"]);
  });
});

describe("environment dumps", () => {
  const dump = [
    "# my notes",
    "DATABASE_PASSWORD=hunter2hunter2",
    "STRIPE_SECRET_KEY=abcdefghijklmnop",
    "PORT=3000",
  ].join("\n");

  it("detects a pasted .env however it is named", () => {
    expect(looksLikeEnvDump(dump)).toBe(true);
    expect(looksLikeEnvDump(`export API_KEY="abcdefgh12345"\nexport DB_TOKEN=zzzzzzzz9999`)).toBe(true);
    expect(findSecretsInFile(Buffer.from(dump))).toContain("environment variable dump");
  });

  it("does not flag a single placeholder or ordinary configuration", () => {
    expect(looksLikeEnvDump("API_KEY=your_api_key_here")).toBe(false);
    expect(looksLikeEnvDump("PORT=3000\nHOST=localhost\nDEBUG=true\nNODE_ENV=production")).toBe(false);
    expect(looksLikeEnvDump("Set the token in the dashboard, then password rotation happens monthly.")).toBe(false);
  });

  it("is quick on a very large file", () => {
    const start = Date.now();
    looksLikeEnvDump("x=1\n".repeat(200_000));
    expect(Date.now() - start).toBeLessThan(2000);
  });
});
