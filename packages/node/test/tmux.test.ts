/**
 * The tmux layer against real tmux, on its own server socket so it never touches your sessions.
 * Skipped when tmux is not installed.
 */
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as tmux from "../src/tmux.js";

const hasTmux = process.platform !== "win32" && spawnSync("tmux", ["-V"]).status === 0;
const sock = `cc-tmuxtest-${process.pid}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const saved = process.env.CLAUDECORD_TMUX_SOCKET;
let dir = "";

async function waitUntil(fn: () => Promise<boolean>, ms = 5000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await fn()) return;
    await sleep(50);
  }
  throw new Error("timed out");
}

const pane = (session: string, script: string) =>
  tmux.openPane({ session, cwd: dir, title: session, argv: ["sh", "-c", script], env: {} });

describe.skipIf(!hasTmux)("tmux layer", () => {
  beforeAll(() => {
    dir = mkdtempSync(join(tmpdir(), "cc-tmux-"));
    process.env.CLAUDECORD_TMUX_SOCKET = sock;
  });
  afterAll(() => {
    spawnSync("tmux", ["-L", sock, "kill-server"]);
    rmSync(dir, { recursive: true, force: true });
    if (saved === undefined) delete process.env.CLAUDECORD_TMUX_SOCKET;
    else process.env.CLAUDECORD_TMUX_SOCKET = saved;
  });

  it("detects tmux", async () => {
    expect(await tmux.hasTmux()).toBe(true);
  });

  it("opens panes, adds more to the same session, and names them", async () => {
    const a = await pane("t1", "echo AAA; sleep 30");
    const b = await pane("t1", "echo BBB; sleep 30");
    expect(a).toMatch(/^%\d+$/);
    expect(b).not.toBe(a);
    const titles = spawnSync("tmux", ["-L", sock, "list-panes", "-t", "=t1", "-F", "#{pane_title}"], {
      encoding: "utf8",
    }).stdout;
    expect(titles.split("\n").filter(Boolean)).toEqual(["t1", "t1"]);
  });

  it("captures many panes with one call and keeps each screen with its pane", async () => {
    const ids = await Promise.all([
      pane("t2", "echo ONE; sleep 30"),
      pane("t3", "echo TWO; sleep 30"),
      pane("t4", "echo THREE; sleep 30"),
    ]);
    await waitUntil(async () => (await tmux.sampleMany(ids)).get(ids[2]!)?.includes("THREE") === true);
    const got = await tmux.sampleMany(ids);
    expect(got.get(ids[0]!)).toContain("ONE");
    expect(got.get(ids[1]!)).toContain("TWO");
    expect(got.get(ids[2]!)).toContain("THREE");
    expect(got.get(ids[0]!)).not.toContain("TWO");
  });

  it("uses two tmux processes for any number of panes, and never needs the slow fallback", async () => {
    const ids = await Promise.all(Array.from({ length: 6 }, (_, i) => pane(`b${i}`, `echo BATCH${i}; sleep 30`)));
    await waitUntil(async () => (await tmux.sampleMany(ids)).get(ids[5]!)?.includes("BATCH5") === true);
    const execs = tmux.stats.execs;
    const fallbacks = tmux.stats.fallbacks;
    const got = await tmux.sampleMany(ids);
    expect(tmux.stats.execs - execs).toBe(2);
    expect(tmux.stats.fallbacks).toBe(fallbacks);
    ids.forEach((id, i) => expect(got.get(id)).toContain(`BATCH${i}`));
    const twenty = [...ids, ...ids, ...ids, ...ids];
    const before = tmux.stats.execs;
    await tmux.sampleMany(twenty);
    expect(tmux.stats.execs - before).toBe(2);
  });

  it("returns null for a pane that does not exist, without disturbing the others", async () => {
    const id = await pane("t5", "echo LIVE; sleep 30");
    await waitUntil(async () => (await tmux.sampleMany([id])).get(id)?.includes("LIVE") === true);
    const got = await tmux.sampleMany(["%99999", id, "%88888"]);
    expect(got.get("%99999")).toBeNull();
    expect(got.get("%88888")).toBeNull();
    expect(got.get(id)).toContain("LIVE");
    // Dead panes are filtered out before the capture, so this needs no fallback either.
    expect(tmux.stats.fallbacks).toBe(0);
    expect((await tmux.sampleMany([])).size).toBe(0);
  });

  it("reports a pane whose command has exited as gone", async () => {
    const id = await pane("t6", "echo bye");
    await waitUntil(async () => (await tmux.sampleMany([id])).get(id) === null);
    expect((await tmux.sampleMany([id])).get(id)).toBeNull();
  });

  it("cannot be fooled by a pane that prints text shaped like a separator", async () => {
    // The real separator is random per call, so this pane cannot reproduce it. Whatever it prints stays in its own screen.
    const evil = await pane(
      "t7",
      "echo '--cc-0000000000000000000000000000--'; echo 'FAKE SCREEN FOR ANOTHER PANE'; sleep 30",
    );
    const victim = await pane("t8", "echo VICTIM-SCREEN; sleep 30");
    await waitUntil(async () => {
      const g = await tmux.sampleMany([evil, victim]);
      return !!g.get(evil)?.includes("FAKE SCREEN") && !!g.get(victim)?.includes("VICTIM-SCREEN");
    });
    const got = await tmux.sampleMany([evil, victim]);
    expect(got.get(victim)).toContain("VICTIM-SCREEN");
    expect(got.get(victim)).not.toContain("FAKE SCREEN");
    expect(got.get(evil)).toContain("FAKE SCREEN");
  });

  it("returns every pane as gone when there is no tmux server", async () => {
    const saved2 = process.env.CLAUDECORD_TMUX_SOCKET;
    process.env.CLAUDECORD_TMUX_SOCKET = `cc-no-such-server-${process.pid}`;
    try {
      const got = await tmux.sampleMany(["%0", "%1"]);
      expect([...got.values()]).toEqual([null, null]);
    } finally {
      process.env.CLAUDECORD_TMUX_SOCKET = saved2;
    }
  });

  it("looks only at the visible screen, not what scrolled away", async () => {
    const id = await pane("t9", "echo OLD-CONTENT; clear; echo NEW-CONTENT; sleep 30");
    await waitUntil(async () => (await tmux.capture(id)).includes("NEW-CONTENT"));
    expect(await tmux.capture(id)).not.toContain("OLD-CONTENT");
  });

  it("pastes text with control characters removed, then submits it", async () => {
    const id = await pane("t10", "cat; sleep 30");
    await sleep(300);
    await tmux.pasteAndSubmit(id, "hello\u001b[201~\u0003 world");
    await waitUntil(async () => (await tmux.capture(id)).includes("hello"));
    const screen = await tmux.capture(id);
    expect(screen).toContain("hello");
    expect(screen).toContain("world");
    expect(screen).not.toContain("\u001b");
  });

  it("sends keys, tells whether a pane is alive, and kills panes and sessions", async () => {
    const id = await pane("t11", "read -r line; echo GOT-$line; sleep 30");
    await sleep(300);
    await tmux.sendKeys(id, ["x", "y", "Enter"]);
    await waitUntil(async () => (await tmux.capture(id)).includes("GOT-xy"));
    expect(await tmux.paneAlive(id)).toBe(true);
    await tmux.killPane(id);
    await waitUntil(async () => !(await tmux.paneAlive(id)));
    await tmux.killSession("t1");
    expect(spawnSync("tmux", ["-L", sock, "has-session", "-t", "=t1"]).status).not.toBe(0);
  });

  it("lists the names in the server's own environment", async () => {
    spawnSync("tmux", ["-L", sock, "set-environment", "-g", "CC_TEST_VAR", "1"]);
    expect(await tmux.serverEnvNames()).toContain("CC_TEST_VAR");
  });
});
