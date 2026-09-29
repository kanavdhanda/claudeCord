/**
 * The Discord layer, run against a fake Discord. The fake implements only the surface the bridge uses, so these tests
 * show what the bridge asks Discord to do and how it reacts to what Discord sends. They do not prove Discord itself
 * behaves that way, which only a live server can.
 */
import { EventEmitter } from "node:events";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ChannelType } from "discord.js";
import type { HubConfig } from "../src/config.js";
import { Auth } from "../src/auth.js";
import { Db } from "../src/db.js";
import { DiscordBridge, type DiscordDeps } from "../src/discord.js";
import { Hub } from "../src/hub.js";

const OWNER = "223456789012345678";
const GUILD = "123456789012345678";

class Coll<T> extends Map<string, T> {
  find(fn: (v: T) => boolean): T | undefined {
    for (const v of this.values()) if (fn(v)) return v;
    return undefined;
  }
}

interface Sent {
  [k: string]: unknown;
}

function fakeMessage(id: string) {
  const reactions = new Map<string, { users: { remove: (id: string) => Promise<void> } }>();
  const removed: string[] = [];
  const msg = {
    id,
    content: "",
    edits: [] as string[],
    pinned: false,
    reacted: [] as string[],
    removed,
    reactions: { cache: reactions },
    edit: async (c: string) => void msg.edits.push(c),
    pin: async () => void (msg.pinned = true),
    react: async (e: string) => void msg.reacted.push(e),
  };
  return msg;
}

type FakeMsg = ReturnType<typeof fakeMessage>;

function fakeChannel(id: string, name: string, type: ChannelType, parentId?: string) {
  const messages = new Map<string, FakeMsg>();
  const ch = {
    id,
    name,
    type,
    parentId,
    sent: [] as Sent[],
    webhooks: [] as { id: string; token: string }[],
    typing: 0,
    failTyping: false,
    activeThreads: new Coll<{ id: string; name: string }>(),
    createdThreads: [] as { id: string; name: string }[],
    messages: { fetch: async (mid: string) => messages.get(mid) ?? Promise.reject(new Error("Unknown Message")) },
    messageStore: messages,
    isTextBased: () => true,
    isThread: () => type === ChannelType.PublicThread,
    createWebhook: async () => {
      const w = { id: `wh${ch.webhooks.length + 1}`, token: `tok${ch.webhooks.length + 1}` };
      ch.webhooks.push(w);
      return w;
    },
    send: async (payload: Sent | string) => {
      ch.sent.push(typeof payload === "string" ? { content: payload } : payload);
      const m = fakeMessage(`m${messages.size + 1}`);
      messages.set(m.id, m);
      return m;
    },
    sendTyping: async () => {
      if (ch.failTyping) throw new Error("rate limited");
      ch.typing++;
    },
    threads: {
      fetchActive: async () => ({ threads: ch.activeThreads }),
      create: async (o: { name: string }) => {
        const t = { id: `th${ch.createdThreads.length + 1}`, name: o.name };
        ch.createdThreads.push(t);
        ch.activeThreads.set(t.id, t);
        return t;
      },
    },
  };
  return ch;
}

type FakeChan = ReturnType<typeof fakeChannel>;

function fakeWorld(opts: { flags?: boolean; perms?: string[]; guildMissing?: boolean } = {}) {
  const channels = new Coll<FakeChan>();
  const all = new Map<string, FakeChan>();
  let n = 0;
  const add = (name: string, type: ChannelType, parent?: string) => {
    const c = fakeChannel(`c${++n}`, name, type, parent);
    channels.set(c.id, c);
    all.set(c.id, c);
    return c;
  };
  const perms = new Set(
    opts.perms ?? [
      "ViewChannel",
      "ManageChannels",
      "ManageWebhooks",
      "SendMessages",
      "AddReactions",
      "AttachFiles",
      "CreatePublicThreads",
      "SendMessagesInThreads",
      "ReadMessageHistory",
      "ManageMessages",
    ],
  );
  const guild = {
    name: "Test Server",
    channels: {
      fetch: async () => channels,
      create: async (o: { name: string; type: ChannelType; parent?: string }) => add(o.name, o.type, o.parent),
    },
    members: { fetchMe: async () => ({ permissions: { has: (k: string) => perms.has(k) } }) },
  };
  const client = Object.assign(new EventEmitter(), {
    user: { id: "bot1", tag: "claudecord#0001" },
    application: { fetch: async () => ({ flags: { has: () => opts.flags !== false } }) },
    guilds: { fetch: async () => (opts.guildMissing ? Promise.reject(new Error("Unknown Guild")) : guild) },
    channels: { fetch: async (id: string) => all.get(id) ?? null },
    login: async () => void setImmediate(() => client.emit("clientReady")),
    destroy: async () => {},
  });
  const hooks: { id: string; token: string; sends: Sent[] }[] = [];
  const registered: { appId: string; guildId: string; body: Record<string, unknown>[] }[] = [];
  const deps: DiscordDeps = {
    client: client as unknown as DiscordDeps["client"],
    webhook: (id, token) => {
      const h = { id, token, sends: [] as Sent[] };
      hooks.push(h);
      return { send: async (p: Sent) => void h.sends.push(p) } as never;
    },
    putCommands: async (appId, guildId, body) => void registered.push({ appId, guildId, body: body as never }),
  };
  return { client, guild, add, channels, all, deps, hooks, registered };
}

function setup(cfgOver: Partial<HubConfig> = {}, worldOpts: Parameters<typeof fakeWorld>[0] = {}) {
  const cfg: HubConfig = {
    discordToken: "x".repeat(70),
    guildId: GUILD,
    ownerId: OWNER,
    publicUrl: "https://hub.example.com",
    categoryName: "claudecord",
    port: 0,
    dbPath: ":memory:",
    ...cfgOver,
  };
  const db = new Db(":memory:");
  const hub = new Hub(db);
  const auth = new Auth(db);
  const world = fakeWorld(worldOpts);
  const bridge = new DiscordBridge(cfg, hub, auth, world.deps);
  hub.out = bridge;
  return { cfg, db, hub, auth, world, bridge };
}

const conns = new Map<string, { nodeName: string; sent: unknown[]; send: (f: unknown) => void }>();
async function register(hub: Hub, node: string, project: string, name: string) {
  let c = conns.get(`${node}`);
  if (!c || !hub.nodes.has(node)) {
    const sent: unknown[] = [];
    c = { nodeName: node, sent, send: (f) => void sent.push(f) };
    conns.set(node, c);
    hub.nodeConnected(c as never);
  }
  await hub.onNodeFrame(c as never, {
    t: "agent.register",
    cwd: "/x",
    agent: { agentId: `${project}/${name}`, name, project, adapter: "claude" },
  });
  return c;
}

beforeEach(() => conns.clear());

describe("startup", () => {
  it("logs in, registers owner-only guild commands, and waits for the client to be ready", async () => {
    const s = setup();
    await s.bridge.start();
    expect(s.world.registered).toHaveLength(1);
    const r = s.world.registered[0]!;
    expect(r.appId).toBe("bot1");
    expect(r.guildId).toBe(GUILD);
    const names = r.body.map((c) => c.name).sort();
    expect(names).toEqual(
      [
        "agents",
        "connect",
        "dashboard",
        "devices",
        "killall",
        "lead",
        "pause",
        "resume",
        "revoke",
        "spawn",
        "status",
        "stop",
        "tasks",
      ].sort(),
    );
    // Hidden from regular members, and only usable in a server.
    for (const c of r.body) {
      expect(c.default_member_permissions).toBe("0");
      expect(c.contexts).toEqual([0]);
    }
    expect(names).not.toContain("token");
  });
});

describe("selfCheck", () => {
  it("reports nothing when the bot is set up correctly", async () => {
    const s = setup();
    expect(await s.bridge.selfCheck()).toEqual([]);
  });

  it("names the missing permissions", async () => {
    const s = setup({}, { perms: ["ViewChannel", "SendMessages"] });
    const problems = await s.bridge.selfCheck();
    expect(problems).toHaveLength(1);
    expect(problems[0]).toContain("Test Server");
    for (const p of [
      "Manage Channels",
      "Manage Webhooks",
      "Add Reactions",
      "Attach Files",
      "Create Public Threads",
      "Manage Messages",
    ]) {
      expect(problems[0]).toContain(p);
    }
    expect(problems[0]).not.toContain("View Channels");
  });

  it("says when the Message Content intent is off", async () => {
    const s = setup({}, { flags: false });
    expect((await s.bridge.selfCheck()).join()).toContain("Message Content intent");
  });

  it("says when the server cannot be seen", async () => {
    const s = setup({}, { guildMissing: true });
    expect((await s.bridge.selfCheck()).join()).toContain(`ID ${GUILD}`);
  });
});

describe("channels and webhooks", () => {
  it("creates the category, the channel and a webhook for a new project, once", async () => {
    const s = setup();
    await s.hub.out.ensureProject("my-app");
    await s.hub.out.ensureProject("my-app");
    const created = [...s.world.channels.values()];
    expect(created.map((c) => [c.name, c.type])).toEqual([
      ["claudecord", ChannelType.GuildCategory],
      ["my-app", ChannelType.GuildText],
    ]);
    expect(created[1]!.parentId).toBe(created[0]!.id);
    expect(created[1]!.webhooks).toHaveLength(1);
    expect(s.db.getProject("my-app")?.webhook_token).toBe("tok1");
  });

  it("reuses an existing category and channel instead of duplicating them", async () => {
    const s = setup();
    const cat = s.world.add("claudecord", ChannelType.GuildCategory);
    const existing = s.world.add("my-app", ChannelType.GuildText, cat.id);
    await s.hub.out.ensureProject("my-app");
    expect([...s.world.channels.values()]).toHaveLength(2);
    expect(s.db.getProject("my-app")?.channel_id).toBe(existing.id);
  });

  it("turns an awkward project name into a valid channel name", async () => {
    const s = setup();
    await s.hub.out.ensureProject("My App.v2");
    expect([...s.world.channels.values()].some((c) => c.name === "my-app-v2")).toBe(true);
  });
});

describe("posting as agents", () => {
  async function ready() {
    const s = setup();
    await register(s.hub, "mac", "alpha", "otter");
    return { ...s, agent: s.db.getAgent("alpha/otter")!, hook: () => s.world.hooks[0]! };
  }

  it("posts under the agent's name, mentioning only the owner, with no avatar by default", async () => {
    const s = await ready();
    await s.hub.out.post("alpha", s.agent, "hello team");
    const sent = s.hook().sends[0]!;
    expect(sent.content).toBe("hello team");
    expect(sent.username).toBe("otter (claude)");
    expect(sent.avatarURL).toBeUndefined();
    expect(sent.allowedMentions).toEqual({ users: [OWNER] });
  });

  it("uses an avatar only when a template is configured, and encodes the name", async () => {
    const s = setup({ avatarTemplate: "https://img.example/{name}.png" });
    await register(s.hub, "mac", "alpha", "otter");
    await s.hub.out.post("alpha", s.db.getAgent("alpha/otter")!, "hi");
    expect(s.world.hooks[0]!.sends[0]!.avatarURL).toBe("https://img.example/otter.png");
  });

  it("splits a long message into chunks under Discord's limit", async () => {
    const s = await ready();
    await s.hub.out.post("alpha", s.agent, "x".repeat(4500));
    const sends = s.hook().sends;
    expect(sends).toHaveLength(3);
    expect(sends.every((m) => (m.content as string).length <= 1900)).toBe(true);
    expect(sends.map((m) => m.content).join("")).toHaveLength(4500);
  });

  it("opens a thread once per topic and reuses it", async () => {
    const s = await ready();
    await s.hub.out.post("alpha", s.agent, "one", "schema");
    await s.hub.out.post("alpha", s.agent, "two", "schema");
    await s.hub.out.post("alpha", s.agent, "three", "api");
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    expect(ch.createdThreads.map((t) => t.name)).toEqual(["schema", "api"]);
    expect(s.hook().sends.map((m) => m.threadId)).toEqual(["th1", "th1", "th2"]);
  });

  it("finds an existing active thread instead of creating a second one", async () => {
    const s = await ready();
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    ch.activeThreads.set("old", { id: "old", name: "schema" });
    await s.hub.out.post("alpha", s.agent, "hi", "schema");
    expect(ch.createdThreads).toHaveLength(0);
    expect(s.hook().sends[0]!.threadId).toBe("old");
  });

  it("asks a question with numbered options and pings the owner", async () => {
    const s = await ready();
    await s.hub.out.postAsk("alpha", s.agent, {
      askId: "q",
      agentId: "alpha/otter",
      question: "which db?",
      options: ["postgres", "sqlite"],
    });
    const sent = s.hook().sends[0]!;
    expect(sent.content).toBe(`<@${OWNER}>`);
    const embed = (sent.embeds as { data: { description: string; footer: { text: string } } }[])[0]!.data;
    expect(embed.description).toContain("which db?");
    expect(embed.description).toContain("1. postgres");
    expect(embed.description).toContain("2. sqlite");
    expect(embed.footer.text).toContain("@otter");
  });

  it("posts a report with artifacts and the completion marker", async () => {
    const s = await ready();
    await s.hub.out.postReport("alpha", s.agent, "Export done", "All endpoints in.", ["pr/12", "docs/export.md"]);
    const sent = s.hook().sends[0]!;
    expect(sent.content).toContain("[STATUS: COMPLETE]");
    const data = (sent.embeds as { data: { title: string; fields: { value: string }[] } }[])[0]!.data;
    expect(data.title).toBe("Export done");
    expect(data.fields[0]!.value).toContain("pr/12");
  });

  it("posts a file as an attachment with its caption", async () => {
    const s = await ready();
    await s.hub.out.postFile("alpha", s.agent, "notes.txt", Buffer.from("hello"), "the notes");
    const sent = s.hook().sends[0]!;
    expect(sent.content).toBe("the notes");
    expect((sent.files as { name: string; attachment: Buffer }[])[0]).toMatchObject({ name: "notes.txt" });
  });

  it("posts notices as the bot, and only pings the owner when asked", async () => {
    const s = await ready();
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    await s.hub.out.notice("alpha", "heron joined");
    await s.hub.out.notice("alpha", "hit a limit", true);
    const [a, b] = ch.sent.filter((m) => /heron joined|hit a limit/.test(String(m.content)));
    expect(a!.content).toBe("heron joined");
    expect(a!.allowedMentions).toEqual({ users: [] });
    expect(b!.content).toBe(`<@${OWNER}> hit a limit`);
    expect(b!.allowedMentions).toEqual({ users: [OWNER] });
  });
});

describe("acceptance reaction", () => {
  it("swaps the seen reaction for the accepted one", async () => {
    const s = setup();
    await register(s.hub, "mac", "alpha", "otter");
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    const msg = fakeMessage("h1");
    const removed: string[] = [];
    msg.reactions.cache.set("\u{1F440}", { users: { remove: async (id) => void removed.push(id) } });
    ch.messageStore.set("h1", msg);
    await s.hub.out.confirm("alpha", `${ch.id}:h1`, "otter");
    expect(removed).toEqual(["bot1"]);
    expect(msg.reacted).toEqual(["✅"]);
  });

  it("ignores a malformed reference and a message that no longer exists", async () => {
    const s = setup();
    await register(s.hub, "mac", "alpha", "otter");
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    await expect(s.hub.out.confirm("alpha", "garbage", "otter")).resolves.toBeUndefined();
    await expect(s.hub.out.confirm("alpha", `${ch.id}:gone`, "otter")).rejects.toThrow();
  });
});

describe("status board", () => {
  // setImmediate stays real, because the fake client's login uses it to signal readiness.
  beforeEach(() =>
    vi.useFakeTimers({ toFake: ["setTimeout", "setInterval", "clearTimeout", "clearInterval", "Date"] }),
  );
  afterEach(() => vi.useRealTimers());

  it("posts and pins one status message, then edits it, after a short debounce", async () => {
    const s = setup();
    await register(s.hub, "mac", "alpha", "otter");
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    s.hub.out.refreshStatus("alpha");
    s.hub.out.refreshStatus("alpha");
    s.hub.out.refreshStatus("alpha");
    await vi.advanceTimersByTimeAsync(1600);
    const boards = ch.sent.filter((m) => String(m.content).startsWith("Status"));
    expect(boards).toHaveLength(1);
    expect(String(boards[0]!.content)).toContain("otter [lead] - claude on mac");
    const pinned = [...ch.messageStore.values()].find((m) => m.pinned);
    expect(pinned).toBeDefined();
    s.hub.out.refreshStatus("alpha");
    await vi.advanceTimersByTimeAsync(1600);
    expect(ch.sent.filter((m) => String(m.content).startsWith("Status"))).toHaveLength(1);
    expect(pinned!.edits).toHaveLength(1);
  });

  it("posts a fresh board when the old one was deleted", async () => {
    const s = setup();
    await register(s.hub, "mac", "alpha", "otter");
    const ch = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    s.hub.out.refreshStatus("alpha");
    await vi.advanceTimersByTimeAsync(1600);
    ch.messageStore.clear();
    s.hub.out.refreshStatus("alpha");
    await vi.advanceTimersByTimeAsync(1600);
    expect(ch.sent.filter((m) => String(m.content).startsWith("Status"))).toHaveLength(2);
  });

  it("shows typing only for projects with a busy agent, and survives a failure", async () => {
    const s = setup();
    await s.bridge.start();
    const c = await register(s.hub, "mac", "alpha", "otter");
    await register(s.hub, "mac", "beta", "finch");
    await s.hub.onNodeFrame(c as never, { t: "agent.status", agentId: "alpha/otter", status: "thinking" });
    const alpha = s.world.all.get(s.db.getProject("alpha")!.channel_id)!;
    const beta = s.world.all.get(s.db.getProject("beta")!.channel_id)!;
    await vi.advanceTimersByTimeAsync(8100);
    expect(alpha.typing).toBe(1);
    expect(beta.typing).toBe(0);
    alpha.failTyping = true;
    await vi.advanceTimersByTimeAsync(8100);
    expect(alpha.typing).toBe(1);
  });
});

// Inbound

function incoming(over: Record<string, unknown> = {}) {
  const replies: Sent[] = [];
  const reacted: string[] = [];
  return {
    id: "mid1",
    content: "hello",
    channelId: "",
    webhookId: null,
    author: { bot: false, id: OWNER },
    channel: { isThread: () => false, parentId: undefined as string | undefined, name: "" },
    attachments: new Coll<{ name: string; size: number; url: string }>(),
    replies,
    reacted,
    reply: async (p: Sent) => void replies.push(p),
    react: async (e: string) => void reacted.push(e),
    ...over,
  };
}

async function started() {
  const s = setup();
  await s.bridge.start();
  const c = await register(s.hub, "mac", "alpha", "otter");
  const project = s.db.getProject("alpha")!;
  return {
    ...s,
    c,
    project,
    send: (m: unknown) => s.world.client.emit("messageCreate", m),
    settle: () => new Promise((r) => setImmediate(r)),
  };
}

describe("messages from you", () => {
  it("routes a message in a project channel to the lead and marks it seen", async () => {
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "build it" });
    t.send(m);
    await t.settle();
    expect(m.reacted).toEqual(["\u{1F440}"]);
    const delivered = t.c.sent.filter(
      (f) => (f as { t: string; from: string }).t === "deliver" && (f as { from: string }).from === "engineer",
    );
    expect(delivered).toHaveLength(1);
    expect((delivered[0] as { text: string }).text).toBe("build it");
  });

  it("uses the thread name when the message is in a thread of the project channel", async () => {
    const t = await started();
    const m = incoming({
      channelId: "thread9",
      content: "in thread",
      channel: { isThread: () => true, parentId: t.project.channel_id, name: "schema" },
    });
    t.send(m);
    await t.settle();
    const f = t.c.sent.find((x) => (x as { from: string }).from === "engineer") as { thread: string };
    expect(f.thread).toBe("schema");
  });

  it.each([
    ["a bot", { author: { bot: true, id: OWNER } }],
    ["a webhook", { webhookId: "w1" }],
    ["someone else", { author: { bot: false, id: "999999999999999999" } }],
    ["an empty message", { content: "   " }],
  ])("ignores %s", async (_n, over) => {
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, ...over });
    t.send(m);
    await t.settle();
    expect(m.reacted).toEqual([]);
    expect(m.replies).toEqual([]);
    expect(t.c.sent.filter((f) => (f as { from: string }).from === "engineer")).toHaveLength(0);
  });

  it("ignores messages outside any project channel", async () => {
    const t = await started();
    const m = incoming({ channelId: "some-other-channel" });
    t.send(m);
    await t.settle();
    expect(m.reacted).toEqual([]);
    expect(m.replies).toEqual([]);
  });

  it("says so when nobody is connected to the project", async () => {
    const t = await started();
    t.hub.db.removeAgent("alpha/otter");
    const m = incoming({ channelId: t.project.channel_id });
    t.send(m);
    await t.settle();
    expect(String(m.replies[0]!.content)).toContain("No agents are connected");
    expect(m.reacted).toEqual([]);
  });

  it("says so when the addressed device is offline, and does not claim delivery", async () => {
    const t = await started();
    t.hub.nodeDisconnected(t.c as never);
    const m = incoming({ channelId: t.project.channel_id });
    t.send(m);
    await t.settle();
    expect(String(m.replies[0]!.content)).toContain("otter not connected");
    expect(m.reacted).toEqual([]);
  });

  it("tells you when the agent is paused and the message is queued", async () => {
    const t = await started();
    t.hub.hold(true, "alpha");
    const m = incoming({ channelId: t.project.channel_id });
    t.send(m);
    await t.settle();
    expect(m.reacted).toEqual(["\u{1F440}"]);
    expect(String(m.replies[0]!.content)).toContain("otter is paused");
  });
});

describe("attachments", () => {
  const cdn = "https://cdn.discordapp.com/attachments/1/2/data.csv";

  afterEach(() => vi.unstubAllGlobals());

  const stubFetch = (impl: () => Promise<unknown>) => {
    const f = vi.fn(impl);
    vi.stubGlobal("fetch", f);
    return f;
  };

  it("downloads from Discord's CDN and sends the file to the addressed agent", async () => {
    const f = stubFetch(async () => ({
      ok: true,
      arrayBuffer: async () => new TextEncoder().encode("a,b\n1,2\n").buffer,
    }));
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "" });
    m.attachments.set("a", { name: "data.csv", size: 10, url: cdn });
    t.send(m);
    await t.settle();
    await t.settle();
    expect(f).toHaveBeenCalledWith(cdn, { redirect: "error" });
    expect(t.c.sent.some((x) => (x as { t: string }).t === "file.chunk")).toBe(true);
    expect(m.reacted).toEqual(["\u{1F440}"]);
  });

  it("refuses to fetch from anywhere that is not Discord, and says why", async () => {
    const f = stubFetch(async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(1) }));
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "" });
    m.attachments.set("a", { name: "x.bin", size: 10, url: "http://169.254.169.254/latest/meta-data" });
    t.send(m);
    await t.settle();
    expect(f).not.toHaveBeenCalled();
    expect(String(m.replies[0]!.content)).toContain("not hosted on Discord");
    expect(m.reacted).toEqual([]);
  });

  it("refuses a file over the limit", async () => {
    const f = stubFetch(async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(1) }));
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "" });
    m.attachments.set("a", { name: "huge.bin", size: 11 * 1024 * 1024, url: cdn });
    t.send(m);
    await t.settle();
    expect(f).not.toHaveBeenCalled();
    expect(String(m.replies[0]!.content)).toContain("10 MB limit");
  });

  it("reports a download that fails instead of failing silently", async () => {
    stubFetch(async () => ({ ok: false, status: 404 }));
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "" });
    m.attachments.set("a", { name: "data.csv", size: 10, url: cdn });
    t.send(m);
    await t.settle();
    await t.settle();
    expect(String(m.replies[0]!.content)).toContain("Could not download data.csv");
    stubFetch(async () => Promise.reject(new Error("redirect")));
    const m2 = incoming({ channelId: t.project.channel_id, content: "" });
    m2.attachments.set("a", { name: "x.csv", size: 10, url: cdn });
    t.send(m2);
    await t.settle();
    await t.settle();
    expect(String(m2.replies[0]!.content)).toContain("Could not download x.csv");
  });

  it("still routes the text when a message has both text and a file", async () => {
    stubFetch(async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) }));
    const t = await started();
    const m = incoming({ channelId: t.project.channel_id, content: "see attached" });
    m.attachments.set("a", { name: "data.csv", size: 4, url: cdn });
    t.send(m);
    await t.settle();
    await t.settle();
    expect(t.c.sent.some((x) => (x as { from?: string; text?: string }).text === "see attached")).toBe(true);
  });
});

// Slash commands

function slash(name: string, opts: Record<string, string> = {}, over: Record<string, unknown> = {}) {
  const replies: { content: string; ephemeral?: boolean }[] = [];
  return {
    isChatInputCommand: () => true,
    commandName: name,
    user: { id: OWNER },
    channelId: "",
    channel: { isThread: () => false, parentId: undefined as string | undefined },
    options: { getString: (n: string) => opts[n] ?? null },
    replies,
    reply: async (p: { content: string; ephemeral?: boolean }) => void replies.push(p),
    ...over,
  };
}

describe("slash commands", () => {
  async function run(name: string, opts: Record<string, string> = {}, inProject = true) {
    const t = await started();
    const i = slash(name, opts, { channelId: inProject ? t.project.channel_id : "elsewhere" });
    t.world.client.emit("interactionCreate", i);
    await t.settle();
    await t.settle();
    return { ...t, i, last: () => i.replies.at(-1)! };
  }

  it("refuses everyone but the owner", async () => {
    const t = await started();
    const i = slash("killall", {}, { user: { id: "999999999999999999" } });
    t.world.client.emit("interactionCreate", i);
    await t.settle();
    expect(i.replies).toEqual([{ content: "Not allowed.", ephemeral: true }]);
    expect(t.c.sent.some((f) => (f as { t: string }).t === "killall")).toBe(false);
  });

  it("ignores interactions that are not slash commands", async () => {
    const t = await started();
    t.world.client.emit("interactionCreate", { isChatInputCommand: () => false });
    await t.settle();
    expect(t.c.sent.filter((f) => (f as { t: string }).t !== "deliver")).toEqual([]);
  });

  it("/connect gives a one-time code and the exact command, privately", async () => {
    const r = await run("connect");
    expect(r.last().ephemeral).toBe(true);
    const m = r.last().content.match(/npx claudecord login (\S+) ([A-Z0-9]{4}-[A-Z0-9]{4})/);
    expect(m?.[1]).toBe("https://hub.example.com");
    expect(r.auth.redeemPairCode(m![2]!, "laptop")).not.toBeNull();
    expect(r.auth.redeemPairCode(m![2]!, "laptop")).toBeNull();
  });

  it("/dashboard gives a one-time sign-in link, privately", async () => {
    const r = await run("dashboard");
    expect(r.last().ephemeral).toBe(true);
    const token = r.last().content.match(/dashboard\/login\?t=(\S+)/)![1]!;
    expect(r.last().content).toContain("https://hub.example.com/dashboard/login");
    expect(r.auth.redeemLoginToken(token)).toBeTruthy();
    expect(r.auth.redeemLoginToken(token)).toBeNull();
  });

  it("/devices lists devices with their state and agent count, and hints when there are none", async () => {
    const empty = await run("devices");
    expect(empty.last().content).toContain("No devices yet");
    const t = await started();
    t.db.createToken("mac");
    t.db.createToken("gpu");
    const i = slash("devices", {}, { channelId: t.project.channel_id });
    t.world.client.emit("interactionCreate", i);
    await t.settle();
    await t.settle();
    expect(i.replies[0]!.content).toMatch(/mac\s+online\s+1 agent/);
    expect(i.replies[0]!.content).toMatch(/gpu\s+offline\s+0 agent/);
    expect(i.replies[0]!.ephemeral).toBe(true);
  });

  it("/revoke revokes a device token", async () => {
    const t = await started();
    const token = t.db.createToken("laptop");
    const i = slash("revoke", { device: "laptop" }, { channelId: t.project.channel_id });
    t.world.client.emit("interactionCreate", i);
    await t.settle();
    expect(i.replies[0]!.content).toBe("Revoked.");
    expect(t.db.nodeForToken(token)).toBeNull();
    const j = slash("revoke", { device: "nope" }, { channelId: t.project.channel_id });
    t.world.client.emit("interactionCreate", j);
    await t.settle();
    expect(j.replies[0]!.content).toBe("No such device.");
  });

  it("/killall, /stop, /pause and /resume reach the device", async () => {
    const t = await started();
    const go = async (name: string, opts: Record<string, string> = {}) => {
      const i = slash(name, opts, { channelId: t.project.channel_id });
      t.world.client.emit("interactionCreate", i);
      await t.settle();
      return i.replies.at(-1)!.content;
    };
    expect(await go("pause")).toBe("Paused 1 agent(s).");
    expect(t.c.sent.some((f) => (f as { t: string; on?: boolean }).t === "hold" && (f as { on: boolean }).on)).toBe(
      true,
    );
    expect(await go("resume")).toBe("Resumed 1 agent(s).");
    expect(await go("stop", { agent: "otter" })).toBe("Stopping.");
    expect(await go("stop", { agent: "ghost" })).toBe("Agent not found or node offline.");
    expect(await go("killall")).toBe("Stopping 1 agent(s).");
    expect(t.c.sent.some((f) => (f as { t: string }).t === "killall")).toBe(true);
  });

  it("/spawn asks the named device to start an agent, and says when it is not connected", async () => {
    const t = await started();
    const go = async (opts: Record<string, string>) => {
      const i = slash("spawn", opts, { channelId: t.project.channel_id });
      t.world.client.emit("interactionCreate", i);
      await t.settle();
      return i.replies.at(-1)!;
    };
    const ok = await go({ node: "mac", name: "heron", adapter: "codex", model: "gpt-5", role: "executor" });
    expect(ok.content).toBe("Spawning heron.");
    const frame = t.c.sent.find((f) => (f as { t: string }).t === "spawn") as { agent: Record<string, string> };
    expect(frame.agent).toMatchObject({
      agentId: "alpha/heron",
      name: "heron",
      adapter: "codex",
      model: "gpt-5",
      role: "executor",
    });
    expect((await go({ node: "nowhere", name: "x" })).content).toBe("Node nowhere is not connected.");
  });

  it("/spawn picks a name when none is given, and rejects an invalid one without contacting the device", async () => {
    const t = await started();
    const go = async (opts: Record<string, string>) => {
      const i = slash("spawn", opts, { channelId: t.project.channel_id });
      t.world.client.emit("interactionCreate", i);
      await t.settle();
      return i.replies.at(-1)!;
    };
    expect((await go({ node: "mac" })).content).toMatch(/^Spawning [a-z]+-[a-z]+\.$/);
    const before = t.c.sent.length;
    const bad = await go({ node: "mac", name: "bad name" });
    expect(bad.content).toContain("Cannot spawn");
    expect(bad.ephemeral).toBe(true);
    const badModel = await go({ node: "mac", name: "ok", model: "x; rm -rf ~" });
    expect(badModel.content).toContain("Cannot spawn");
    expect(t.c.sent.length).toBe(before);
  });

  it("/agents and /status show the project's agents", async () => {
    const r = await run("agents");
    expect(r.last().content).toContain("otter [lead] - claude on mac");
    const s2 = await run("status");
    expect(s2.last().content).toContain("otter");
  });

  it("/tasks shows the board", async () => {
    const t = await started();
    await register(t.hub, "gpu", "alpha", "heron");
    await t.hub.onNodeFrame(t.c as never, {
      t: "agent.assign",
      agentId: "alpha/otter",
      to: "heron",
      task: "write the migration",
    });
    const i = slash("tasks", {}, { channelId: t.project.channel_id });
    t.world.client.emit("interactionCreate", i);
    await t.settle();
    expect(i.replies[0]!.content).toMatch(/T1\s+assigned\s+heron: write the migration/);
    const empty = await run("tasks");
    expect(empty.last().content).toContain("No tasks yet.");
  });

  it("/lead changes the lead and rejects an unknown agent", async () => {
    const t = await started();
    await register(t.hub, "gpu", "alpha", "heron");
    const go = async (agent: string) => {
      const i = slash("lead", { agent }, { channelId: t.project.channel_id });
      t.world.client.emit("interactionCreate", i);
      await t.settle();
      return i.replies.at(-1)!;
    };
    expect((await go("heron")).content).toBe("heron is now lead.");
    expect(t.db.getAgent("alpha/heron")?.is_lead).toBe(1);
    expect((await go("ghost")).content).toBe("Agent not found.");
  });

  it.each(["stop", "spawn", "tasks", "lead"])("/%s outside a project channel asks you to go to one", async (name) => {
    const r = await run(name, { agent: "otter", node: "mac" }, false);
    expect(r.last().content).toContain("inside a project channel");
    expect(r.last().ephemeral).toBe(true);
  });

  it("/agents outside a project channel says so", async () => {
    const r = await run("agents", {}, false);
    expect(r.last().content).toContain("inside a project channel");
  });
});
