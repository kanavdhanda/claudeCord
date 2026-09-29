import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { brotliCompressSync, gzipSync } from "node:zlib";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { request as httpRequest, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { Auth } from "../src/auth.js";
import { Db } from "../src/db.js";
import { startGateway } from "../src/gateway.js";
import { Hub, type Outbound } from "../src/hub.js";
import { TtlCache, createHttpHandler, parseCookies } from "../src/http.js";

const CSS_HASH = "abc123";
let site: string;
let server: Server;
let base: string;
let db: Db;
let hub: Hub;
let auth: Auth;
let publicUrl = "http://localhost:0";

const noop: Outbound = {
  ensureProject: async () => {},
  post: async () => {},
  postAsk: async () => {},
  postReport: async () => {},
  postFile: async () => {},
  confirm: async () => {},
  notice: async () => {},
  refreshStatus: () => {},
};

const html = (title: string) => `<!doctype html><title>${title}</title><h1>${title}</h1>`.repeat(40);

const conns = new Map<string, { nodeName: string; send: () => void }>();
async function register(node: string, project: string, name: string) {
  // A device connects once. Connecting again clears its agents, which is what a reconnect should do.
  let conn = conns.get(node);
  if (!conn) {
    conn = { nodeName: node, send: () => {} };
    conns.set(node, conn);
    hub.nodeConnected(conn);
  }
  await hub.onNodeFrame(conn, {
    t: "agent.register",
    cwd: "/x",
    agent: { agentId: `${project}/${name}`, name, project, adapter: "claude" },
  });
}

/** A raw request, because fetch hides compression and will not let a test choose the Host header. */
function raw(path: string, headers: Record<string, string> = {}) {
  return new Promise<{ status: number; headers: Record<string, string | string[] | undefined>; length: number }>(
    (resolve, reject) => {
      const url = new URL(base);
      const req = httpRequest({ host: url.hostname, port: url.port, path, headers }, (res) => {
        let length = 0;
        res.on("data", (c: Buffer) => (length += c.length));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, headers: res.headers, length }));
      });
      req.on("error", reject);
      req.end();
    },
  );
}

beforeAll(async () => {
  site = mkdtempSync(join(tmpdir(), "cc-site-"));
  mkdirSync(join(site, "setup"), { recursive: true });
  mkdirSync(join(site, "dashboard"), { recursive: true });
  mkdirSync(join(site, "assets"), { recursive: true });
  const index = html("home");
  writeFileSync(join(site, "index.html"), index);
  writeFileSync(join(site, "index.html.br"), brotliCompressSync(Buffer.from(index)));
  writeFileSync(join(site, "index.html.gz"), gzipSync(Buffer.from(index)));
  writeFileSync(join(site, "setup", "index.html"), html("setup"));
  writeFileSync(join(site, "dashboard", "index.html"), html("dashboard"));
  writeFileSync(join(site, "404.html"), html("not found"));
  writeFileSync(join(site, "assets", "app.abc123.js"), "console.log(1)");
  writeFileSync(join(site, "robots.txt"), "User-agent: *\nAllow: /\n");
  writeFileSync(join(site, "manifest.json"), JSON.stringify({ cssHash: CSS_HASH, pages: ["/", "/setup/"] }));

  db = new Db(":memory:");
  hub = new Hub(db);
  hub.out = noop;
  auth = new Auth(db);
  server = startGateway(hub, 0, createHttpHandler({ hub, auth, publicUrl: () => publicUrl, siteDir: site }));
  await new Promise((r) => server.once("listening", r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;

  for (let i = 0; i < 30; i++)
    await register(i % 2 ? "mac" : "gpu", `proj${i % 6}`, `agent${String(i).padStart(2, "0")}`);
  db.createToken("mac");
  db.createToken("gpu");
});

afterAll(() => {
  server.close();
  rmSync(site, { recursive: true, force: true });
});

const get = (path: string, headers: Record<string, string> = {}) => fetch(base + path, { headers, redirect: "manual" });

async function signIn(): Promise<string> {
  const { token } = auth.createLoginToken();
  const res = await get(`/dashboard/login?t=${token}`);
  return res.headers.get("set-cookie")!.split(";")[0]!;
}

describe("static site", () => {
  it("serves pages with strict security headers", async () => {
    const res = await get("/");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/html; charset=utf-8");
    const csp = res.headers.get("content-security-policy")!;
    expect(csp).toContain("default-src 'none'");
    expect(csp).toContain("script-src 'self'");
    expect(csp).toContain(`'sha256-${CSS_HASH}'`);
    expect(csp).not.toContain("unsafe-inline");
    expect(csp).toContain("frame-ancestors 'none'");
    expect(res.headers.get("x-content-type-options")).toBe("nosniff");
    expect(res.headers.get("x-frame-options")).toBe("DENY");
    expect(res.headers.get("referrer-policy")).toBe("strict-origin-when-cross-origin");
    expect(res.headers.get("permissions-policy")).toContain("camera=()");
  });

  it("serves precompressed brotli, then gzip, then plain", async () => {
    const plain = await raw("/", { "accept-encoding": "identity" });
    const br = await raw("/", { "accept-encoding": "br, gzip" });
    const gz = await raw("/", { "accept-encoding": "gzip" });
    expect(plain.headers["content-encoding"]).toBeUndefined();
    expect(br.headers["content-encoding"]).toBe("br");
    expect(gz.headers["content-encoding"]).toBe("gzip");
    expect(String(br.headers.vary)).toContain("Accept-Encoding");
    expect(br.length).toBeLessThan(plain.length / 5);
    expect(gz.length).toBeLessThan(plain.length / 5);
  });

  it("caches hashed assets forever and pages briefly", async () => {
    expect((await get("/assets/app.abc123.js")).headers.get("cache-control")).toBe(
      "public, max-age=31536000, immutable",
    );
    expect((await get("/assets/app.abc123.js")).headers.get("content-type")).toContain("text/javascript");
    expect((await get("/")).headers.get("cache-control")).toContain("max-age=300");
  });

  it("answers 304 to a matching ETag", async () => {
    const first = await get("/setup/");
    const etag = first.headers.get("etag")!;
    expect(etag).toBeTruthy();
    const again = await get("/setup/", { "if-none-match": etag });
    expect(again.status).toBe(304);
    expect((await again.text()).length).toBe(0);
  });

  it("supports HEAD without a body", async () => {
    const res = await fetch(`${base}/`, { method: "HEAD" });
    expect(res.status).toBe(200);
    expect(Number(res.headers.get("content-length"))).toBeGreaterThan(0);
    expect((await res.text()).length).toBe(0);
  });

  it("uses clean slugs: one canonical form, and old forms redirect to it", async () => {
    expect((await get("/setup")).status).toBe(301);
    expect((await get("/setup")).headers.get("location")).toBe("/setup/");
    expect((await get("/setup/index.html")).headers.get("location")).toBe("/setup/");
    expect((await get("/setup.html")).headers.get("location")).toBe("/setup/");
    expect((await get("/index.html")).headers.get("location")).toBe("/");
    expect((await get("/setup/")).status).toBe(200);
  });

  it("returns a real 404 that search engines will not index", async () => {
    const res = await get("/no-such-page/");
    expect(res.status).toBe(404);
    expect(res.headers.get("x-robots-tag")).toContain("noindex");
    expect(await res.text()).toContain("not found");
  });

  it("serves robots.txt as text", async () => {
    const res = await get("/robots.txt");
    expect(res.headers.get("content-type")).toContain("text/plain");
    expect(await res.text()).toContain("User-agent");
  });

  it.each([
    "/..%2f..%2f..%2fetc%2fpasswd",
    "/%2e%2e/%2e%2e/etc/passwd",
    "/assets/..%2f..%2f..%2fetc%2fhosts",
    "/%00",
    "/..\\..\\etc\\passwd",
  ])("will not serve files outside the site: %s", async (path) => {
    const res = await get(path);
    expect([400, 403, 404]).toContain(res.status);
    expect(await res.text()).not.toContain("root:");
  });

  it("only allows GET and HEAD on pages", async () => {
    const res = await fetch(`${base}/`, { method: "POST", body: "x" });
    expect(res.status).toBe(405);
    expect(res.headers.get("allow")).toContain("GET");
  });

  it("keeps the dashboard shell and API out of search results", async () => {
    expect((await get("/dashboard/")).headers.get("x-robots-tag")).toContain("noindex");
    expect((await get("/api/v1/dash/summary")).headers.get("x-robots-tag")).toContain("noindex");
  });
});

describe("https enforcement", () => {
  it("redirects plain http to https when the hub's public URL is https, except for health checks and localhost", async () => {
    publicUrl = "https://hub.example.com";
    try {
      const res = await raw("/setup/", { "x-forwarded-proto": "http", host: "hub.example.com" });
      expect(res.status).toBe(308);
      expect(res.headers.location).toBe("https://hub.example.com/setup/");
      expect((await raw("/healthz", { "x-forwarded-proto": "http", host: "hub.example.com" })).status).toBe(200);
      expect((await raw("/", { "x-forwarded-proto": "http", host: "localhost:8787" })).status).toBe(200);
      expect((await raw("/", { "x-forwarded-proto": "https", host: "hub.example.com" })).status).toBe(200);
      // A request that already arrived over https is never bounced, so there is no redirect loop behind a proxy.
      expect((await raw("/setup/", { "x-forwarded-proto": "https", host: "hub.example.com" })).status).toBe(200);
    } finally {
      publicUrl = "http://localhost:0";
    }
  });

  it("sends HSTS only over https", async () => {
    expect((await get("/", { "x-forwarded-proto": "https" })).headers.get("strict-transport-security")).toContain(
      "max-age=63072000",
    );
    expect((await get("/")).headers.get("strict-transport-security")).toBeNull();
  });
});

describe("pairing endpoint", () => {
  const pair = (body: unknown, raw = false) =>
    fetch(`${base}/api/v1/pair`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: raw ? (body as string) : JSON.stringify(body),
    });

  it("exchanges a one-time code for a device token and the hub address", async () => {
    const { code } = auth.createPairCode();
    const res = await pair({ code, device: "newbox" });
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as { token: string; device: string; hub: string };
    expect(body.device).toBe("newbox");
    expect(body.hub).toBe("ws://localhost:0");
    expect(db.nodeForToken(body.token)).toBe("newbox");
    expect((await pair({ code, device: "newbox" })).status).toBe(401);
  });

  it("rejects malformed requests", async () => {
    expect((await pair("{ not json", true)).status).toBe(400);
    expect((await pair({ code: 5, device: "x" })).status).toBe(400);
    expect((await pair({})).status).toBe(400);
    expect((await fetch(`${base}/api/v1/pair`)).status).toBe(405);
  });

  it("rejects an oversized body", async () => {
    const res = await pair(JSON.stringify({ code: "x".repeat(10_000), device: "a" }), true).catch(() => null);
    expect(res === null || res.status >= 400).toBe(true);
  });

  it("locks an address out after repeated wrong codes, even for a valid code afterwards", async () => {
    let last = 0;
    for (let i = 0; i < 12; i++) last = (await pair({ code: "AAAA-AAAA", device: "x" })).status;
    expect(last).toBe(429);
    const { code } = auth.createPairCode();
    const res = await pair({ code, device: "x" });
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).toBe("60");
  });
});

describe("dashboard", () => {
  it("needs a session for every API route", async () => {
    for (const r of ["summary", "agents", "tasks", "devices"])
      expect((await get(`/api/v1/dash/${r}`)).status).toBe(401);
    expect((await get("/api/v1/dash/agents", { cookie: "cc_session=forged" })).status).toBe(401);
  });

  it("signs in with a one-time link, sets a locked-down cookie, and the link works once", async () => {
    const { token } = auth.createLoginToken();
    const res = await get(`/dashboard/login?t=${token}`);
    expect(res.status).toBe(303);
    expect(res.headers.get("location")).toBe("/dashboard/");
    const cookie = res.headers.get("set-cookie")!;
    expect(cookie).toContain("HttpOnly");
    expect(cookie).toContain("SameSite=Strict");
    expect(cookie).toContain("Path=/");
    expect(cookie).not.toContain("Secure");
    const again = await get(`/dashboard/login?t=${token}`);
    expect(again.headers.get("location")).toContain("expired");
    expect(again.headers.get("set-cookie")).toBeNull();
  });

  it("marks the cookie Secure over https", async () => {
    const { token } = auth.createLoginToken();
    const res = await get(`/dashboard/login?t=${token}`, { "x-forwarded-proto": "https" });
    expect(res.headers.get("set-cookie")).toContain("Secure");
  });

  it("summarises the system", async () => {
    const cookie = await signIn();
    const res = await get("/api/v1/dash/summary", { cookie });
    expect(res.status).toBe(200);
    const s = (await res.json()) as { agents: { total: number }; projects: number; devices: { total: number } };
    expect(s.agents.total).toBe(30);
    expect(s.projects).toBe(6);
    expect(s.devices.total).toBeGreaterThanOrEqual(2);
  });

  it("paginates, searches and filters agents", async () => {
    const cookie = await signIn();
    const page = async (q: string) =>
      (await (await get(`/api/v1/dash/agents${q}`, { cookie })).json()) as {
        items: { name: string; project: string }[];
        page: number;
        pages: number;
        total: number;
      };
    const p1 = await page("?limit=10");
    expect(p1.items).toHaveLength(10);
    expect(p1.total).toBe(30);
    expect(p1.pages).toBe(3);
    const p3 = await page("?limit=10&page=3");
    expect(p3.items).toHaveLength(10);
    expect(new Set([...p1.items, ...p3.items].map((a) => a.name)).size).toBe(20);
    expect((await page("?limit=10&page=99")).page).toBe(3);
    expect((await page("?q=agent07")).items.map((a) => a.name)).toEqual(["agent07"]);
    expect((await page("?q=PROJ3")).total).toBe(5);
    expect((await page("?status=offline")).total).toBe(0);
    expect((await page("?limit=1000")).items.length).toBe(30);
    expect((await page("?limit=-5&page=abc")).items.length).toBeGreaterThan(0);
  });

  it("sorts agents by project then name", async () => {
    const cookie = await signIn();
    const body = (await (await get("/api/v1/dash/agents?limit=100", { cookie })).json()) as {
      items: { name: string; project: string }[];
    };
    const keys = body.items.map((a) => `${a.project}/${a.name}`);
    expect(keys).toEqual([...keys].sort((a, b) => a.localeCompare(b)));
  });

  it("lists devices and tasks", async () => {
    const cookie = await signIn();
    const devices = (await (await get("/api/v1/dash/devices", { cookie })).json()) as {
      name: string;
      online: boolean;
      agents: number;
    }[];
    expect(devices.find((d) => d.name === "mac")?.agents).toBe(15);
    const tasks = (await (await get("/api/v1/dash/tasks", { cookie })).json()) as { items: unknown[]; total: number };
    expect(tasks.total).toBe(0);
    expect((await get("/api/v1/dash/nope", { cookie })).status).toBe(404);
  });

  it("compresses big responses and supports conditional requests", async () => {
    const cookie = await signIn();
    const small = await raw("/api/v1/dash/summary", { cookie, "accept-encoding": "br" });
    expect(small.headers["content-encoding"]).toBeUndefined();
    const plain = await raw("/api/v1/dash/agents?limit=100", { cookie, "accept-encoding": "identity" });
    const big = await raw("/api/v1/dash/agents?limit=100", { cookie, "accept-encoding": "br, gzip" });
    expect(big.headers["content-encoding"]).toBe("br");
    expect(big.length).toBeLessThan(plain.length / 3);
    expect(big.headers["cache-control"]).toBe("private, no-cache");
    const etag = String(big.headers.etag);
    expect(etag).toMatch(/^W\//);
    expect((await get("/api/v1/dash/agents?limit=100", { cookie, "if-none-match": etag })).status).toBe(304);
  });

  it("is read only, and sign out needs the custom header and ends the session", async () => {
    const cookie = await signIn();
    expect((await fetch(`${base}/api/v1/dash/agents`, { method: "POST", headers: { cookie } })).status).toBe(405);
    expect((await fetch(`${base}/api/v1/dash/logout`, { method: "POST", headers: { cookie } })).status).toBe(403);
    expect(
      (await fetch(`${base}/api/v1/dash/logout`, { headers: { cookie, "x-requested-with": "claudecord" } })).status,
    ).toBe(403);
    const out = await fetch(`${base}/api/v1/dash/logout`, {
      method: "POST",
      headers: { cookie, "x-requested-with": "claudecord" },
    });
    expect(out.status).toBe(204);
    expect(out.headers.get("set-cookie")).toContain("Max-Age=0");
    expect((await get("/api/v1/dash/summary", { cookie })).status).toBe(401);
  });

  it("locks out an address that keeps trying bad sign-in links", async () => {
    let last = "";
    for (let i = 0; i < 14; i++) {
      const r = await get("/dashboard/login?t=bogus");
      last = r.status === 429 ? "429" : (r.headers.get("location") ?? "");
    }
    expect(last).toBe("429");
  });
});

describe("helpers", () => {
  it("parses cookies", () => {
    expect(parseCookies("a=1; cc_session=abc%3D; b=2")).toEqual({ a: "1", cc_session: "abc=", b: "2" });
    expect(parseCookies(undefined)).toEqual({});
  });

  it("expires cache entries and stays bounded", () => {
    const c = new TtlCache<number>(1000, 3);
    c.set("a", 1, 0);
    expect(c.get("a", 500)).toBe(1);
    expect(c.get("a", 1500)).toBeUndefined();
    for (let i = 0; i < 10; i++) c.set(`k${i}`, i, 0);
    let present = 0;
    for (let i = 0; i < 10; i++) if (c.get(`k${i}`, 0) !== undefined) present++;
    expect(present).toBe(3);
  });
});
