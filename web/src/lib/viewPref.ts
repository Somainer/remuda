const prefix = "runtime.session-view.";

export type SessionView = "tty" | "structured";

export function readSessionView(instanceId: string): SessionView | null {
  try {
    const raw = localStorage.getItem(prefix + instanceId);
    return raw === "tty" || raw === "structured" ? raw : null;
  } catch {
    return null;
  }
}

export function writeSessionView(instanceId: string, view: SessionView): void {
  try {
    localStorage.setItem(prefix + instanceId, view);
  } catch {
    /* ignore quota */
  }
}
