const prefix = "runtime.draft.";

export function readDraft(instanceId: string): string {
  try {
    return localStorage.getItem(prefix + instanceId) ?? "";
  } catch {
    return "";
  }
}

export function writeDraft(instanceId: string, text: string): void {
  try {
    if (!text) localStorage.removeItem(prefix + instanceId);
    else localStorage.setItem(prefix + instanceId, text);
  } catch {
    /* ignore quota */
  }
}
