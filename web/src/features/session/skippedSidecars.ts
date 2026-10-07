/**
 * Resume-staging skipped-sidecar notices (c-resumehome round 6 item 8).
 *
 * When a resume is staged into a fresh native home, non-regular/symlinked
 * sidecar entries (FIFOs, sockets, devices, links) are deliberately NOT
 * copied. The Node journals each omission as a non-fatal native lifecycle
 * diagnostic (`severity: warning`, `affects_completion: false`).
 *
 * The UI renders these as a neutral NOTICE, never as a session failure: the
 * conversation still runs to completion.
 */

/** One journaled skipped-sidecar notice. */
export type SkippedSidecarNotice = {
  /** `"kind:project-relative/path"` entries the staging walk skipped. */
  entries: string[];
  /** Journal event id, when the envelope carries one. */
  eventId?: string;
  /** Envelope time, when present. */
  observedAt?: string;
};

const NATIVE_NAME = "resume_staging";
const STATUS_VALUE = "skipped-sidecars";
const RELATED_KEY = "skippedSidecars";

/** Split the Node's comma-joined relatedIds value into clean entries. */
export function parseSkippedEntries(raw: unknown): string[] {
  if (typeof raw !== "string") return [];
  return raw
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

/**
 * Extract every skipped-sidecar diagnostic from a journal event window, in
 * journal order. Empty notices (no parseable entries) are dropped. Malformed
 * events are ignored, never thrown on.
 */
export function skippedSidecarNotices(events: readonly unknown[]): SkippedSidecarNotice[] {
  const out: SkippedSidecarNotice[] = [];
  for (const event of events) {
    const e = event as
      | {
          eventId?: string;
          observedAt?: string;
          kind?: string;
          payload?: {
            type?: string;
            topic?: string;
            nativeName?: string;
            status?: { value?: unknown };
            relatedIds?: Record<string, unknown>;
          };
        }
      | null;
    const p = e?.payload;
    if (
      e?.kind !== "lifecycle" ||
      p?.type !== "native" ||
      p.topic !== "diagnostic" ||
      p.nativeName !== NATIVE_NAME ||
      p.status?.value !== STATUS_VALUE
    ) {
      continue;
    }
    const entries = parseSkippedEntries(p.relatedIds?.[RELATED_KEY]);
    if (entries.length === 0) continue;
    out.push({
      entries,
      ...(typeof e.eventId === "string" ? { eventId: e.eventId } : {}),
      ...(typeof e.observedAt === "string" ? { observedAt: e.observedAt } : {}),
    });
  }
  return out;
}

/**
 * The deduplicated set of skipped entries across a window, in first-seen
 * order — what a single summary chip shows.
 */
export function skippedSidecarEntries(events: readonly unknown[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const notice of skippedSidecarNotices(events)) {
    for (const entry of notice.entries) {
      if (!seen.has(entry)) {
        seen.add(entry);
        out.push(entry);
      }
    }
  }
  return out;
}
