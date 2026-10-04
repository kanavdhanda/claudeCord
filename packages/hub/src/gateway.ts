import { createServer, type RequestListener, type Server } from "node:http";
import { WebSocketServer } from "ws";
import { NODE_CONNECT_PATH, encode, parseNodeFrame, type HubFrame } from "@claudecord/protocol";
import type { Hub, NodeConn } from "./hub.js";
import { Bucket, FailureLimiter } from "./limits.js";

/** One chunk of a file is about 256 KB on the wire, so nothing legitimate comes close to this. */
const MAX_FRAME_BYTES = 1024 * 1024;
/** A node may burst this many frames, then sustain this many per second. Ordinary agents send a few per second. */
const FRAME_BURST = 600;
const FRAME_RATE = 300;

export function startGateway(hub: Hub, port: number, handler?: RequestListener): Server {
  const failures = new FailureLimiter();
  const server = createServer(
    handler ??
      ((req, res) => {
        res.writeHead(req.url === "/healthz" ? 200 : 404).end(req.url === "/healthz" ? "ok" : undefined);
      }),
  );
  // Bound what a stalled or hostile client can hold open.
  server.headersTimeout = 10_000;
  server.requestTimeout = 30_000;
  const wss = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME_BYTES });

  const refuse = (socket: import("node:stream").Duplex, status: string) => {
    socket.write(`HTTP/1.1 ${status}\r\nConnection: close\r\n\r\n`);
    socket.destroy();
  };

  server.on("upgrade", (req, socket, head) => {
    const ip = req.socket.remoteAddress ?? "unknown";
    if (failures.blocked(ip)) return refuse(socket, "429 Too Many Requests");
    const url = new URL(req.url ?? "/", "http://x");
    const auth = req.headers.authorization ?? "";
    const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
    const nodeName = url.pathname === NODE_CONNECT_PATH && token ? hub.db.nodeForToken(token) : null;
    if (!nodeName) {
      failures.fail(ip);
      return refuse(socket, "401 Unauthorized");
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      const conn: NodeConn = {
        nodeName,
        send: (f: HubFrame) => ws.readyState === ws.OPEN && ws.send(encode(f)),
      };
      hub.nodeConnected(conn);
      conn.send({ t: "welcome", nodeId: nodeName });
      const alive = setInterval(() => ws.ping(), 20_000);
      const bucket = new Bucket(FRAME_BURST, FRAME_RATE);
      // Handle one node's frames strictly in order, so a say right after a register is not dropped.
      let chain: Promise<void> = Promise.resolve();
      ws.on("message", (data) => {
        if (!bucket.take()) {
          ws.close(1008, "rate limit exceeded");
          return;
        }
        const f = parseNodeFrame(data.toString());
        if (!f) return conn.send({ t: "error", message: "bad frame" });
        chain = chain.then(() => hub.onNodeFrame(conn, f)).catch((e) => console.error("frame error", e));
      });
      ws.on("close", () => {
        clearInterval(alive);
        hub.nodeDisconnected(conn);
      });
      ws.on("error", () => ws.close());
    });
  });

  server.listen(port, () => console.log(`gateway listening on :${port}`));
  return server;
}
