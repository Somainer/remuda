/**
 * Per-instance reading position and follow state (workbench batch E, §5 P1-2).
 *
 * "退出恢复阅读位置和跟随状态": leaving a session and opening it again must
 * bring the reader back to the same row, and a transcript that was pinned to
 * the latest must be pinned again. Storage is the existing `runtime.*`
 * localStorage namespace: the only data written is the instance id (already
 * in every route this page uses), a node anchor id (opaque journal id), a
 * pixel offset and a boolean. No prompt text, no journal content, no native
 * session id — nothing sensitive beyond what drafts.ts already keeps.
 */

const KEY_PREFIX = "runtime.reading.v1.";

export type ReadingPosition = {
  /** Stable node id to restore (assemble.ts keeps ids stable across appends). */
  anchorId: string;
  /** Pixels below the anchor row's top, so tall rows restore to the same line. */
  offset: number;
  /**
   * Scroll ratio in [0,1]: the anchor's offset divided by the estimated total
   * content height computed with {@link avgRow}. Raw pixels drift badly
   * between visits because unmounted rows are all placeholders; restoring
   * with the same per-row average the visit measured is scale-invariant and
   * is refined against real heights as rows mount.
   */
  ratio: number;
  /** Average measured row height at save time; fall back to the default. */
  avgRow: number;
  /** Whether the transcript was pinned to the latest event. */
  follow: boolean;
};

export function readPosition(instanceId: string): ReadingPosition | null {
  try {
    const raw = localStorage.getItem(KEY_PREFIX + instanceId);
    if (!raw) return null;
    const value = JSON.parse(raw) as Partial<ReadingPosition>;
    if (typeof value.anchorId !== "string" || typeof value.follow !== "boolean") return null;
    const ratio = typeof value.ratio === "number" && Number.isFinite(value.ratio) ? Math.min(1, Math.max(0, value.ratio)) : 0;
    const avgRow = typeof value.avgRow === "number" && Number.isFinite(value.avgRow) && value.avgRow > 0 ? value.avgRow : 0;
    return {
      anchorId: value.anchorId,
      offset: Number.isFinite(value.offset) ? Number(value.offset) : 0,
      ratio,
      avgRow,
      follow: value.follow,
    };
  } catch {
    return null;
  }
}

export function writePosition(instanceId: string, position: ReadingPosition): void {
  try {
    localStorage.setItem(KEY_PREFIX + instanceId, JSON.stringify(position));
  } catch {
    /* private mode / quota: position simply does not persist */
  }
}

export function clearPosition(instanceId: string): void {
  try {
    localStorage.removeItem(KEY_PREFIX + instanceId);
  } catch {
    /* ignore */
  }
}
