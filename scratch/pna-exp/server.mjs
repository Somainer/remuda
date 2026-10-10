// Minimal same-origin server: static page+SW and a real WebSocket endpoint.
// Logs every upgrade so we can tell PNA block (no arrival) from other errors.
import http from "node:http";
import https from "node:https";
import fs from "node:fs";
import crypto from "node:crypto";
import { networkInterfaces } from "node:os";

const host = process.env.HOST ?? "127.0.0.1";
const port = Number(process.env.PORT ?? 8099);
const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const PAGE = `<!doctype html><html><body>
<div id="out"></div><script type="module">
const log = (m) => { document.getElementById("out").textContent = m; };
window.__run = async () => {
  return await new Promise((resolve) => {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(\`\${proto}://\${location.host}/ws\`);
    const t = setTimeout(() => resolve("TIMEOUT"), 5000);
    ws.addEventListener("open", () => { clearTimeout(t); resolve("OPEN"); });
    ws.addEventListener("error", () => {
      // error fires before close; let close give the code.
    });
    ws.addEventListener("close", (e) => {
      clearTimeout(t);
      resolve("CLOSE:" + e.code);
    });
  });
};
log("ready");
</script></body></html>`;

const SW = `self.addEventListener("install", (e) => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  if (event.request.method !== "GET") return;
  event.respondWith((async () => {
    const cache = await caches.open("pna-exp");
    try {
      const res = await fetch(event.request);
      if (res.ok) cache.put(event.request, res.clone());
      return res;
    } catch {
      const hit = await cache.match(event.request);
      if (hit) return hit;
      throw new Error("offline");
    }
  })());
});`;

const makeServer = process.env.TLS === "1"
  ? https.createServer({ key: fs.readFileSync("/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/scratch/pna-exp/key.pem"), cert: fs.readFileSync("/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/scratch/pna-exp/cert.pem") })
  : http.createServer();
const server = makeServer;
server.on("request", (req, res) => {
  if (req.url === "/sw.js") {
    res.writeHead(200, { "Content-Type": "application/javascript", "Service-Worker-Allowed": "/" });
    res.end(SW);
    return;
  }
  res.writeHead(200, { "Content-Type": "text/html" });
  res.end(PAGE);
});

server.on("upgrade", (req, socket) => {
  const key = req.headers["sec-websocket-key"];
  console.log("UPGRADE_ARRIVED " + req.url);
  const accept = crypto.createHash("sha1").update(key + WS_GUID).digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
      accept + "\r\n\r\n",
  );
  // keep open 6 s so the client sees OPEN
  setTimeout(() => socket.end(), 6000);
});

server.listen(port, host, () => console.log("LISTENING http://" + host + ":" + port));
