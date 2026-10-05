/**
 * Global follow socket for Hub settlement notices (c-cardsettle).
 *
 * When an instance ends, the Hub invalidates its still-pending interactions in
 * the SAME transaction and publishes one settlement notice per card on the
 * `/v1/follow` bus. A settlement frame is a Hub-side CONTROL notice — NOT a
 * journal observation: it carries no seq, must never be inserted into a
 * journal, and cannot mark a journal stale/duplicate. The store pins the
 * interaction and refreshes its list (trailing-coalesced) so every mounted
 * inbox/badge drops the card immediately instead of waiting for the next 2 s
 * poll.
 *
 * Missed notices are self-healing: the next poll (and every reload) reads the
 * durable invalidated rows, so a dropped socket cannot strand a card.
 * Reconnects with the same 1 s backoff as the workspace follow.
 */
export function followSettlements(
  url: string,
  onSettlement: (interactionId: string) => void,
) {
  let stopped = false;
  let socket: WebSocket;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const connect = () => {
    if (stopped) return;
    socket = new WebSocket(url);
    socket.addEventListener("message", (message) => {
      if (typeof message.data !== "string") return;
      try {
        const frame = JSON.parse(message.data) as {
          type?: string;
          state?: string;
          interactionId?: string;
        };
        if (frame.type === "settlement" && frame.state === "invalidated" && frame.interactionId) {
          onSettlement(frame.interactionId);
        }
      } catch {
        /* ignore unrelated or malformed follow frames */
      }
    });
    socket.addEventListener("close", () => {
      if (!stopped) timer = setTimeout(connect, 1000);
    });
  };
  connect();
  return () => {
    stopped = true;
    clearTimeout(timer);
    socket.close();
  };
}
