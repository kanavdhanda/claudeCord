import { createServer, type Server } from "node:http";
import { WebSocketServer } from "ws";
import { NODE_CONNECT_PATH, encode, parseNodeFrame, type HubFrame } from "@claudecord/protocol";
import type { Hub, NodeConn } from "./hub.js";

export function startGateway(hub: Hub, port: number): Server {
  const server = createServer((req, res) => {
    if (req.url === "/healthz") {
      res.writeHead(200).end("ok");
      return;
    }
    res.writeHead(404).end();
  });
  const wss = new WebSocketServer({ noServer: true });

  server.on("upgrade", (req, socket, head) => {
    const url = new URL(req.url ?? "/", "http://x");
    const auth = req.headers.authorization ?? "";
    const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
    const nodeName = url.pathname === NODE_CONNECT_PATH && token ? hub.db.nodeForToken(token) : null;
    if (!nodeName) {
      socket.write("HTTP/1.1 401 Unauthorized\r\n\r\n");
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      const conn: NodeConn = {
        nodeName,
        send: (f: HubFrame) => ws.readyState === ws.OPEN && ws.send(encode(f)),
      };
      hub.nodeConnected(conn);
      conn.send({ t: "welcome", nodeId: nodeName });
      const alive = setInterval(() => ws.ping(), 20_000);
      // Handle one node's frames strictly in order, so a say right after a register is not dropped.
      let chain: Promise<void> = Promise.resolve();
      ws.on("message", (data) => {
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
