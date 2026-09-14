import type { Observation } from "../types/generated";

/**
 * Deterministic transcript binding state for a promoted shell-pty session
 * (D-025). Derived solely from the driver's own `transcript_bound` /
 * `transcript_unbound` / `transcript_degraded` native lifecycles — never from
 * file mtime guessing.
 */
export type TranscriptBindingState =
  | { state: "unbound" }
  | { state: "bound"; sessionId: string; source: string; transcriptPath: string | null }
  | {
      state: "degraded";
      sessionId: string | null;
      source: string | null;
      reason: string | null;
    };

const BOUND = "transcript_bound";
const UNBOUND = "transcript_unbound";
const DEGRADED = "transcript_degraded";

type NativeLifecycleLike = {
  type?: unknown;
  nativeName?: unknown;
  nativeId?: unknown;
  status?: unknown;
  relatedIds?: unknown;
};

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function knowledgeText(value: unknown): string | null {
  if (typeof value === "string" && value.length > 0) return value;
  const rec = asRecord(value);
  if (rec && typeof rec.value === "string" && rec.value.length > 0) return rec.value;
  return null;
}

/** Last `transcript_*` lifecycle wins; each epoch re-announces its state. */
export function transcriptBinding(events: Observation[]): TranscriptBindingState | null {
  let result: TranscriptBindingState | null = null;
  for (const event of events) {
    if (event.kind !== "lifecycle") continue;
    const payload = asRecord(event.payload) ?? asRecord(event);
    if (!payload || payload.type !== "native") continue;
    const lifecycle = payload as unknown as NativeLifecycleLike;
    const related = asRecord(lifecycle.relatedIds) ?? {};
    const relatedString = (key: string): string | null => {
      const value = related[key];
      return typeof value === "string" && value.length > 0 ? value : null;
    };
    switch (lifecycle.nativeName) {
      case BOUND: {
        const sessionId =
          relatedString("sessionId") ?? knowledgeText(lifecycle.nativeId) ?? "";
        if (!sessionId) break;
        result = {
          state: "bound",
          sessionId,
          source: relatedString("source") ?? "unknown",
          transcriptPath: relatedString("transcriptPath"),
        };
        break;
      }
      case UNBOUND:
        result = { state: "unbound" };
        break;
      case DEGRADED:
        result = {
          state: "degraded",
          sessionId: relatedString("sessionId"),
          source: relatedString("source"),
          reason: relatedString("reason") ?? knowledgeText(lifecycle.status),
        };
        break;
      default:
        break;
    }
  }
  return result;
}

const SOURCE_LABELS: Record<string, string> = {
  hook: "hook",
  pid: "pid 文件",
  argv: "argv",
  manual: "手动选择",
};

export function bindingSourceLabel(source: string): string {
  return SOURCE_LABELS[source] ?? source;
}

export function shortSessionId(sessionId: string): string {
  return sessionId.slice(0, 8);
}

/** Human-readable chip text for the session header. */
export function bindingChipText(binding: TranscriptBindingState): string {
  switch (binding.state) {
    case "bound":
      return `transcript ${shortSessionId(binding.sessionId)} · ${bindingSourceLabel(binding.source)}`;
    case "unbound":
      return "未绑定 transcript";
    case "degraded": {
      const who = binding.sessionId ? ` ${shortSessionId(binding.sessionId)}` : "";
      return `transcript${who} 已失效`;
    }
  }
}
