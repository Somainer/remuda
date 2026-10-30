import type { SeenView } from "../../lib/api";

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
 *
 * seen-hub: the same global socket carries owner seen-state changes, an
 * `event` frame addressed to instance `""` whose event is `instance.seen`
 * (the shape workspace `host.updated` rides). `onSeen` receives the view;
 * missed ones converge on the next instance poll, like settlements.
 */
export function followSettlements(
  url: string,
  onSettlement: (interactionId: string, reason?: string) => void,
  onSeen?: (view: SeenView) => void,
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
          reason?: string;
          event?: Record<string, unknown> | null;
        };
        if (frame.type === "event") {
          const view = frame.event ? seenViewOf(frame.event) : null;
          if (view) onSeen?.(view);
          return;
        }
        if (frame.type === "settlement" && frame.state === "invalidated" && frame.interactionId) {
          // Carry the Hub's resolution reason through verbatim: a
          // non-process-end settlement (e.g. transcript-picker demotion =
          // agent-demoted) must not be relabelled generation-ended (r6 item 5).
          const reason = typeof frame.reason === "string" && frame.reason ? frame.reason : undefined;
          onSettlement(frame.interactionId, reason);
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

/** An `instance.seen` follow event as a SeenView, or null for anything else. */
export function seenViewOf(event: Record<string, unknown>): SeenView | null {
  if (event.type !== "instance.seen" || typeof event.instanceId !== "string" || typeof event.unseen !== "boolean") {
    return null;
  }
  const seq = (value: unknown) => (typeof value === "string" && /^\d+$/.test(value) ? value : undefined);
  return {
    instanceId: event.instanceId,
    unseen: event.unseen,
    seenSeq: seq(event.seenSeq),
    seenAt: typeof event.seenAt === "string" ? event.seenAt : undefined,
    turnEndSeq: seq(event.turnEndSeq),
  };
}
