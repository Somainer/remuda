/**
 * D-057 §8.4 (ma-push): report this page's visibility on every per-instance
 * `/v1/follow` socket.
 *
 * The Hub suppresses a push about an instance only for a device that follows
 * it with its page visible. A socket reports `{type:"visibility",
 * state:"hidden"|"visible"}` when it opens and on every `visibilitychange`,
 * so a phone whose PWA went to the background is pushed again while a
 * desktop tab left open on the session keeps only itself quiet. A Hub that
 * predates the frame ignores it; a client that never sends it counts as
 * visible.
 */

/** The frame for the document's current visibility. */
export function visibilityFrame(): string {
  const hidden = typeof document !== "undefined" && document.visibilityState === "hidden";
  return JSON.stringify({ type: "visibility", state: hidden ? "hidden" : "visible" });
}

const sockets = new Set<WebSocket>();
let listening = false;

function sendTo(ws: WebSocket): void {
  if (ws.readyState !== WebSocket.OPEN) return;
  try {
    ws.send(visibilityFrame());
  } catch {
    // A socket closing under us: its close listener drops it.
  }
}

function broadcast(): void {
  for (const ws of sockets) sendTo(ws);
}

/**
 * Keep `ws` reporting the page's visibility until it closes. Safe to call
 * before the socket opens: the first report goes out on `open`.
 */
export function reportFollowVisibility(ws: WebSocket): void {
  sockets.add(ws);
  if (!listening && typeof document !== "undefined") {
    document.addEventListener("visibilitychange", broadcast);
    listening = true;
  }
  ws.addEventListener("open", () => sendTo(ws));
  ws.addEventListener("close", () => sockets.delete(ws));
  if (ws.readyState === WebSocket.OPEN) sendTo(ws);
}
