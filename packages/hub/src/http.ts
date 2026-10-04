/**
 * The hub's web surface: the static site, device pairing, dashboard sign-in and a read-only dashboard API.
 *
 * Design notes
 * - Static files are served from memory with strong ETags, precompressed brotli and gzip variants, and immutable
 *   caching for hashed assets, so a page costs a few hundred bytes of work per request.
 * - HTTPS is enforced when the hub's public URL is https. HSTS and a strict content security policy go on everything.
 * - The API is read-only apart from sign out. Sessions are HttpOnly, SameSite=Strict cookies and only their hash is
 *   stored. Expensive aggregations are cached for a moment, and large responses are compressed.
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { extname, join, normalize, sep } from "node:path";
import { brotliCompressSync, gzipSync } from "node:zlib";
import { SESSION_TTL_MS, type Auth } from "./auth.js";
import type { Hub } from "./hub.js";
import { FailureLimiter } from "./limits.js";

export interface HttpDeps {
  hub: Hub;
  auth: Auth;
  /** Public URL of the hub. When it is https, plain http requests are redirected. A function lets tests use a port picked at runtime. */
  publicUrl: string | (() => string);
  /** Built site directory, if there is one. Without it the hub serves only its API. */
  siteDir?: string;
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".webp": "image/webp",
  ".ico": "image/x-icon",
  ".txt": "text/plain; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
  ".xml": "application/xml; charset=utf-8",
  ".webmanifest": "application/manifest+json",
};

interface Cached {
  body: Buffer;
  br?: Buffer;
  gz?: Buffer;
  etag: string;
  type: string;
}

/** A tiny TTL cache, bounded so it cannot grow without limit. */
export class TtlCache<T> {
  private m = new Map<string, { v: T; exp: number }>();
  constructor(
    private ttlMs: number,
    private max = 200,
  ) {}
  get(key: string, now = Date.now()): T | undefined {
    const e = this.m.get(key);
    if (!e) return undefined;
    if (e.exp < now) {
      this.m.delete(key);
      return undefined;
    }
    return e.v;
  }
  set(key: string, v: T, now = Date.now()): T {
    if (this.m.size >= this.max) this.m.delete(this.m.keys().next().value as string);
    this.m.set(key, { v, exp: now + this.ttlMs });
    return v;
  }
}

const SESSION_COOKIE = "cc_session";

export function parseCookies(header: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of (header ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

function protoOf(req: IncomingMessage): "http" | "https" {
  const fwd = (req.headers["x-forwarded-proto"] as string | undefined)?.split(",")[0]?.trim();
  if (fwd === "https" || fwd === "http") return fwd;
  return (req.socket as { encrypted?: boolean }).encrypted ? "https" : "http";
}

const isLocalHost = (host: string | undefined) => /^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/.test(host ?? "");

export function createHttpHandler(deps: HttpDeps) {
  const { hub, auth } = deps;
  const publicUrl = () => (typeof deps.publicUrl === "function" ? deps.publicUrl() : deps.publicUrl);
  const pairFailures = new FailureLimiter(10, 60_000);
  const loginFailures = new FailureLimiter(10, 60_000);
  const apiCache = new TtlCache<unknown>(2000);
  const files = new Map<string, Cached>();
  let cssHash = "";
  let manifestPages = new Set<string>();

  const siteDir = deps.siteDir && existsSync(deps.siteDir) ? deps.siteDir : undefined;
  if (siteDir && existsSync(join(siteDir, "manifest.json"))) {
    const m = JSON.parse(readFileSync(join(siteDir, "manifest.json"), "utf8")) as { cssHash?: string; pages?: string[] };
    cssHash = m.cssHash ?? "";
    manifestPages = new Set(m.pages ?? []);
  }
  void manifestPages;

  const csp = [
    "default-src 'none'",
    "script-src 'self'",
    `style-src 'self'${cssHash ? ` 'sha256-${cssHash}'` : ""}`,
    "img-src 'self' data:",
    "connect-src 'self'",
    "manifest-src 'self'",
    "base-uri 'none'",
    "form-action 'self'",
    "frame-ancestors 'none'",
  ].join("; ");

  function securityHeaders(req: IncomingMessage, privatePage: boolean): Record<string, string> {
    const h: Record<string, string> = {
      "Content-Security-Policy": csp,
      "X-Content-Type-Options": "nosniff",
      "X-Frame-Options": "DENY",
      "Referrer-Policy": "strict-origin-when-cross-origin",
      "Permissions-Policy": "camera=(), microphone=(), geolocation=(), interest-cohort=()",
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Resource-Policy": "same-origin",
    };
    if (protoOf(req) === "https") h["Strict-Transport-Security"] = "max-age=63072000; includeSubDomains; preload";
    if (privatePage) h["X-Robots-Tag"] = "noindex, nofollow, noarchive";
    return h;
  }

  function send(req: IncomingMessage, res: ServerResponse, status: number, body: Buffer | string, headers: Record<string, string>, priv = false) {
    const buf = typeof body === "string" ? Buffer.from(body) : body;
    res.writeHead(status, { ...securityHeaders(req, priv), "Content-Length": buf.length, ...headers });
    res.end(req.method === "HEAD" ? undefined : buf);
  }

  /** Compresses a response body when the client accepts it and it is big enough to be worth it. */
  function encode(req: IncomingMessage, body: Buffer): { body: Buffer; headers: Record<string, string> } {
    const ae = String(req.headers["accept-encoding"] ?? "");
    const headers: Record<string, string> = { Vary: "Accept-Encoding" };
    if (body.length < 1024) return { body, headers };
    if (/\bbr\b/.test(ae)) return { body: brotliCompressSync(body), headers: { ...headers, "Content-Encoding": "br" } };
    if (/\bgzip\b/.test(ae)) return { body: gzipSync(body), headers: { ...headers, "Content-Encoding": "gzip" } };
    return { body, headers };
  }

  function json(req: IncomingMessage, res: ServerResponse, status: number, data: unknown, extra: Record<string, string> = {}) {
    const raw = Buffer.from(JSON.stringify(data));
    const etag = `W/"${createHash("sha1").update(raw).digest("base64url")}"`;
    const base = { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "private, no-cache", ETag: etag, ...extra };
    if (status === 200 && req.headers["if-none-match"] === etag) {
      res.writeHead(304, { ...securityHeaders(req, true), ...base });
      return res.end();
    }
    const enc = encode(req, raw);
    send(req, res, status, enc.body, { ...base, ...enc.headers }, true);
  }

  const clientIp = (req: IncomingMessage) => req.socket.remoteAddress ?? "unknown";

  async function readBody(req: IncomingMessage, max = 2048): Promise<string | null> {
    return new Promise((resolve) => {
      let n = 0;
      const chunks: Buffer[] = [];
      req.on("data", (c: Buffer) => {
        n += c.length;
        if (n > max) {
          resolve(null);
          req.destroy();
        } else chunks.push(c);
      });
      req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
      req.on("error", () => resolve(null));
    });
  }

  // Pairing

  async function pair(req: IncomingMessage, res: ServerResponse) {
    const ip = clientIp(req);
    const noStore = { "Cache-Control": "no-store" };
    if (pairFailures.blocked(ip)) return json(req, res, 429, { error: "too many attempts, wait a minute" }, { ...noStore, "Retry-After": "60" });
    const body = await readBody(req);
    let parsed: { code?: unknown; device?: unknown } = {};
    try {
      parsed = body ? (JSON.parse(body) as typeof parsed) : {};
    } catch {
      return json(req, res, 400, { error: "invalid JSON" }, noStore);
    }
    if (typeof parsed.code !== "string" || typeof parsed.device !== "string") return json(req, res, 400, { error: "code and device are required" }, noStore);
    const out = auth.redeemPairCode(parsed.code, parsed.device);
    if (!out) {
      pairFailures.fail(ip);
      return json(req, res, 401, { error: "that code is wrong, used or expired. Run /connect in Discord for a new one" }, noStore);
    }
    const hubUrl = publicUrl().replace(/^http/, "ws");
    json(req, res, 200, { token: out.token, device: out.device, hub: hubUrl }, noStore);
  }

  // Dashboard

  const sessionOf = (req: IncomingMessage) => parseCookies(req.headers.cookie)[SESSION_COOKIE];

  function cookie(req: IncomingMessage, value: string, maxAgeS: number): string {
    const secure = protoOf(req) === "https" ? "; Secure" : "";
    return `${SESSION_COOKIE}=${encodeURIComponent(value)}; HttpOnly; SameSite=Strict; Path=/${secure}; Max-Age=${maxAgeS}`;
  }

  function dashboardLogin(req: IncomingMessage, res: ServerResponse, url: URL) {
    const ip = clientIp(req);
    if (loginFailures.blocked(ip)) return send(req, res, 429, "Too many attempts. Wait a minute.", { "Content-Type": "text/plain; charset=utf-8", "Retry-After": "60" }, true);
    const session = auth.redeemLoginToken(url.searchParams.get("t") ?? "");
    if (!session) {
      loginFailures.fail(ip);
      return send(req, res, 303, "", { Location: "/dashboard/?expired=1", "Cache-Control": "no-store" }, true);
    }
    send(req, res, 303, "", { Location: "/dashboard/", "Set-Cookie": cookie(req, session, SESSION_TTL_MS / 1000), "Cache-Control": "no-store" }, true);
  }

  function page(url: URL): { page: number; limit: number } {
    const n = (k: string, d: number, min: number, max: number) => Math.min(max, Math.max(min, Number.parseInt(url.searchParams.get(k) ?? "", 10) || d));
    return { page: n("page", 1, 1, 100_000), limit: n("limit", 25, 1, 100) };
  }

  function paged<T>(all: T[], p: { page: number; limit: number }) {
    const pages = Math.max(1, Math.ceil(all.length / p.limit));
    const pageNo = Math.min(p.page, pages);
    return { items: all.slice((pageNo - 1) * p.limit, pageNo * p.limit), page: pageNo, pages, total: all.length };
  }

  function dash(req: IncomingMessage, res: ServerResponse, url: URL, route: string) {
    if (!auth.verifySession(sessionOf(req))) return json(req, res, 401, { error: "unauthorized" });
    const key = `${route}?${url.searchParams.toString()}`;
    let data = apiCache.get(key);
    if (!data) {
      if (route === "summary") {
        const agents = hub.db.allAgents();
        const byStatus: Record<string, number> = {};
        for (const a of agents) {
          const s = hub.status.get(a.agent_id)?.status ?? "offline";
          byStatus[s] = (byStatus[s] ?? 0) + 1;
        }
        const projects = new Set(agents.map((a) => a.project));
        const tasks = { assigned: 0, accepted: 0, done: 0 };
        for (const p of projects) for (const t of hub.db.tasksOfProject(p)) tasks[t.state]++;
        const devices = hub.db.listDevices().filter((d) => !d.revoked);
        data = { devices: { total: devices.length, online: hub.nodes.size }, agents: { total: agents.length, byStatus }, projects: projects.size, tasks };
      } else if (route === "agents") {
        const q = (url.searchParams.get("q") ?? "").trim().toLowerCase().slice(0, 64);
        const status = url.searchParams.get("status");
        const rows = hub.db
          .allAgents()
          .filter((a) => !q || a.name.toLowerCase().includes(q) || a.project.toLowerCase().includes(q))
          .map((a) => ({
            id: a.agent_id, name: a.name, project: a.project, adapter: a.adapter, model: a.model, role: a.role,
            lead: !!a.is_lead, device: a.node_name, status: hub.status.get(a.agent_id)?.status ?? "offline",
          }))
          .filter((a) => !status || a.status === status)
          .sort((a, b) => a.project.localeCompare(b.project) || a.name.localeCompare(b.name));
        data = paged(rows, page(url));
      } else if (route === "tasks") {
        const project = url.searchParams.get("project");
        const projects = project ? [project] : [...new Set(hub.db.allAgents().map((a) => a.project))];
        const rows = projects
          .flatMap((p) => hub.db.tasksOfProject(p))
          .map((t) => ({ id: t.id, project: t.project, to: t.to_agent.split("/").pop(), from: t.from_agent.split("/").pop(), text: t.text, state: t.state, summary: t.summary, updated: t.updated }))
          .sort((a, b) => b.updated - a.updated);
        data = paged(rows, page(url));
      } else if (route === "devices") {
        const counts = new Map<string, number>();
        for (const a of hub.db.allAgents()) counts.set(a.node_name, (counts.get(a.node_name) ?? 0) + 1);
        data = hub.db
          .listDevices()
          .filter((d) => !d.revoked)
          .map((d) => ({ name: d.node_name, online: hub.nodes.has(d.node_name), agents: counts.get(d.node_name) ?? 0 }));
      } else return json(req, res, 404, { error: "not found" });
      apiCache.set(key, data);
    }
    json(req, res, 200, data);
  }

  // Static site

  function load(path: string): Cached | undefined {
    const hit = files.get(path);
    if (hit) return hit;
    if (!existsSync(path) || !statSync(path).isFile()) return undefined;
    const body = readFileSync(path);
    const entry: Cached = {
      body,
      br: existsSync(`${path}.br`) ? readFileSync(`${path}.br`) : undefined,
      gz: existsSync(`${path}.gz`) ? readFileSync(`${path}.gz`) : undefined,
      etag: `"${createHash("sha1").update(body).digest("base64url")}"`,
      type: MIME[extname(path)] ?? "application/octet-stream",
    };
    files.set(path, entry);
    return entry;
  }

  function serveStatic(req: IncomingMessage, res: ServerResponse, pathname: string) {
    if (!siteDir) return send(req, res, 404, "Not found", { "Content-Type": "text/plain; charset=utf-8" });
    let rel: string;
    try {
      rel = decodeURIComponent(pathname);
    } catch {
      return send(req, res, 400, "Bad request", { "Content-Type": "text/plain; charset=utf-8" });
    }
    if (rel.includes("\0")) return send(req, res, 400, "Bad request", { "Content-Type": "text/plain; charset=utf-8" });
    // Old-style URLs go to the clean slug.
    if (rel.endsWith("/index.html")) return send(req, res, 301, "", { Location: rel.slice(0, -"index.html".length) });
    if (rel.endsWith(".html") && rel !== "/404.html") return send(req, res, 301, "", { Location: `${rel.slice(0, -".html".length)}/` });
    const root = normalize(siteDir + sep);
    let file = normalize(join(siteDir, rel));
    if (!file.startsWith(root) && file + sep !== root) return send(req, res, 403, "Forbidden", { "Content-Type": "text/plain; charset=utf-8" });
    let entry: Cached | undefined;
    if (!extname(rel)) {
      // A page: /setup and /setup/ both mean setup/index.html, and the canonical form has the slash.
      if (!rel.endsWith("/") && existsSync(join(file, "index.html"))) return send(req, res, 301, "", { Location: `${rel}/` });
      file = join(file, "index.html");
    }
    entry = load(file);
    let status = 200;
    if (!entry) {
      entry = load(join(siteDir, "404.html"));
      status = 404;
      if (!entry) return send(req, res, 404, "Not found", { "Content-Type": "text/plain; charset=utf-8" });
    }
    const isAsset = rel.startsWith("/assets/");
    const priv = status === 404 || rel.startsWith("/dashboard");
    const ae = String(req.headers["accept-encoding"] ?? "");
    let body = entry.body;
    const headers: Record<string, string> = { "Content-Type": entry.type, ETag: entry.etag, Vary: "Accept-Encoding" };
    if (entry.br && /\bbr\b/.test(ae)) {
      body = entry.br;
      headers["Content-Encoding"] = "br";
    } else if (entry.gz && /\bgzip\b/.test(ae)) {
      body = entry.gz;
      headers["Content-Encoding"] = "gzip";
    }
    headers["Cache-Control"] = isAsset ? "public, max-age=31536000, immutable" : priv ? "no-cache" : "public, max-age=300, stale-while-revalidate=3600";
    if (status === 200 && req.headers["if-none-match"] === entry.etag) {
      res.writeHead(304, { ...securityHeaders(req, priv), ETag: entry.etag, "Cache-Control": headers["Cache-Control"], Vary: "Accept-Encoding" });
      return res.end();
    }
    send(req, res, status, body, headers, priv);
  }

  return (req: IncomingMessage, res: ServerResponse): void => {
    void (async () => {
      try {
        const url = new URL((req.url ?? "/").slice(0, 2048), "http://hub");
        const path = url.pathname;
        if (path === "/healthz") return send(req, res, 200, "ok", { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "no-store" });
        if (publicUrl().startsWith("https://") && protoOf(req) === "http" && !isLocalHost(req.headers.host)) {
          return send(req, res, 308, "", { Location: `https://${req.headers.host ?? new URL(publicUrl()).host}${req.url ?? "/"}` });
        }
        if (path === "/api/v1/pair") {
          if (req.method !== "POST") return json(req, res, 405, { error: "use POST" }, { Allow: "POST" });
          return await pair(req, res);
        }
        if (path === "/dashboard/login" && req.method === "GET") return dashboardLogin(req, res, url);
        if (path === "/api/v1/dash/logout") {
          if (req.method !== "POST" || req.headers["x-requested-with"] !== "claudecord") return json(req, res, 403, { error: "forbidden" });
          auth.endSession(sessionOf(req));
          return send(req, res, 204, "", { "Set-Cookie": cookie(req, "", 0), "Cache-Control": "no-store" }, true);
        }
        if (path.startsWith("/api/v1/dash/")) {
          if (req.method !== "GET" && req.method !== "HEAD") return json(req, res, 405, { error: "read only" }, { Allow: "GET" });
          return dash(req, res, url, path.slice("/api/v1/dash/".length));
        }
        if (path.startsWith("/api/")) return json(req, res, 404, { error: "not found" });
        if (req.method !== "GET" && req.method !== "HEAD") return send(req, res, 405, "Method not allowed", { Allow: "GET, HEAD", "Content-Type": "text/plain; charset=utf-8" });
        return serveStatic(req, res, path);
      } catch (e) {
        console.error("http error", (e as Error).message);
        if (!res.headersSent) send(req, res, 500, "Server error", { "Content-Type": "text/plain; charset=utf-8" });
        else res.end();
      }
    })();
  };
}
