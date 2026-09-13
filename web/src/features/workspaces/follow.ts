import type { WorkspaceSnapshot } from "../../types/workspace";

/** Host workspace events use the global cookie-authenticated follow stream. */
export function followWorkspaces(url: string, onSnapshot: (snapshot: WorkspaceSnapshot) => void, refresh: () => void) {
  let stopped = false;
  let socket: WebSocket;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const connect = () => {
    if (stopped) return;
    socket = new WebSocket(url);
    socket.addEventListener("open", refresh);
    socket.addEventListener("message", (message) => {
      if (typeof message.data !== "string") return;
      try {
        const frame = JSON.parse(message.data);
        if (frame.type === "gap") { refresh(); return; }
        const event = frame.event;
        if (frame.type !== "event" || event?.type !== "host.updated" || typeof event.hostId !== "string"
          || !Number.isSafeInteger(event.workspaceRevision) || !Array.isArray(event.workspaces)) return;
        if (!event.workspaces.every((row: Record<string, unknown> | null) => row && typeof row.workspaceId === "string"
          && row.hostId === event.hostId && typeof row.root === "string")) return;
        onSnapshot(event as WorkspaceSnapshot);
      } catch { /* ignore unrelated or malformed follow frames */ }
    });
    socket.addEventListener("close", () => {
      if (!stopped) timer = setTimeout(connect, 1000);
    });
  };
  connect();
  return () => { stopped = true; clearTimeout(timer); socket.close(); };
}
