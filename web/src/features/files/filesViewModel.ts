//! Pure state projection for the 「工作区当前变更」 view (files-view-contract §3.5–3.7).
//!
//! No React, no fetch here: given a status payload or a typed HTTP failure and
//! the per-entry details already fetched, it produces a view model the
//! component renders. The six §3.6 availability states are kept distinct —
//! nothing collapses into a generic "加载失败".

import type { HubHttpError } from "../../lib/httpError";
import type {
  Digest,
  ScmDiff,
  ScmDiffItem,
  ScmEntry,
  ScmFile,
  ScmStatus,
} from "../../types/scm";

export type { Digest, ScmDiff, ScmDiffItem, ScmEntry, ScmFile, ScmStatus };

/** The exhaustive view phases (§3.6 + §3.7), rendered distinctly by the UI. */
export type FilesPhase =
  | "not-collected"
  | "loading"
  | "clean"
  | "changes"
  | "unsupported"
  | "forbidden"
  | "offline"
  | "missing"
  | "failed";

export interface FilesViewModel {
  phase: FilesPhase;
  status: ScmStatus | null;
  /** Human-readable reason for 不支持 / 权限不足 / generic failure. */
  reason?: string;
  /** §3.7: any open detail was computed against a different HEAD/size. */
  contentChanged: boolean;
}

/**
 * Map a failed status request to its phase. 409 `HOST_OFFLINE` and 422
 * `PLACEMENT_UNSATISFIABLE` both mean the host is not answerable → 离线;
 * 404 is 工作区不存在; 403 is 权限不足. Anything else is a retriable failure,
 * never mislabelled as one of the availability states.
 */
export function phaseFromError(error: unknown): FilesViewModel {
  const http = error as HubHttpError | undefined;
  const status = http?.status ?? 0;
  const code = http?.code ?? "";
  if (code === "HOST_OFFLINE" || status === 409 || code === "PLACEMENT_UNSATISFIABLE" || status === 422) {
    return { phase: "offline", status: null, contentChanged: false };
  }
  if (status === 404) {
    return { phase: "missing", status: null, contentChanged: false };
  }
  if (status === 403) {
    return {
      phase: "forbidden",
      status: null,
      reason: "operator",
      contentChanged: false,
    };
  }
  return {
    phase: "failed",
    status: null,
    reason: http?.message || "network",
    contentChanged: false,
  };
}

/** Project a resolved status envelope into the top-level view model. */
export function projectStatus(status: ScmStatus): FilesViewModel {
  if (status.availability === "unsupported") {
    return {
      phase: "unsupported",
      status,
      reason: status.unsupportedReason || "unknown",
      contentChanged: false,
    };
  }
  if (status.availability === "denied") {
    return {
      phase: "forbidden",
      status,
      reason: status.deniedReason || "permission-denied",
      contentChanged: false,
    };
  }
  return {
    phase: status.entries.length === 0 ? "clean" : "changes",
    status,
    contentChanged: false,
  };
}

/** Detail load state for one entry. */
export type EntryDetailState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "diff"; item: ScmDiffItem; changed: boolean }
  | { kind: "preview"; file: ScmFile; changed: boolean }
  | { kind: "binary" }
  | { kind: "too-large" }
  | { kind: "error"; message: string };

/** Untracked entries are previewed as current bytes; everything else diffs. */
export function isUntracked(entry: ScmEntry): boolean {
  return entry.xy === "??";
}

/**
 * Whether a fetched diff belongs to the snapshot the user is looking at.
 * The list and the detail are separate requests (§3.7): a moved HEAD means the
 * patch may no longer correspond to the listed entry.
 */
export function diffIsStale(status: ScmStatus, diff: ScmDiff): boolean {
  return Boolean(status.headOid && diff.headOid && status.headOid !== diff.headOid);
}

/** A file preview is stale when HEAD moved or the byte size changed since list. */
export function fileIsStale(entry: ScmEntry, status: ScmStatus, file: ScmFile): boolean {
  if (status.headOid && file.headOid && status.headOid !== file.headOid) return true;
  if (entry.sizeBytes != null && file.sizeBytes != null && entry.sizeBytes !== file.sizeBytes) {
    return true;
  }
  return false;
}

/** Project a resolved diff answer into the per-entry detail state. */
export function projectDiff(entry: ScmEntry, status: ScmStatus, diff: ScmDiff): EntryDetailState {
  const item = diff.items.find((candidate) => candidate.path === entry.path) ?? diff.items[0];
  if (!item) return { kind: "error", message: "empty" };
  if (item.binary) return { kind: "binary" };
  return { kind: "diff", item, changed: diffIsStale(status, diff) };
}

/** Project a resolved file answer into the per-entry detail state. */
export function projectFile(entry: ScmEntry, status: ScmStatus, file: ScmFile): EntryDetailState {
  if (file.truncated) return { kind: "too-large" };
  if (file.binary || file.content == null) return { kind: "binary" };
  return { kind: "preview", file, changed: fileIsStale(entry, status, file) };
}

/** Stable Chinese label for an XY/kind code pair. */
export function kindLabel(entry: ScmEntry): string {
  if (entry.xy === "??") return "未跟踪";
  if (entry.kind === "renamed" || entry.kind === "copied") return "重命名";
  if (entry.kind === "added" || entry.xy[0] === "A") return "新增";
  if (entry.kind === "deleted" || entry.xy.includes("D")) return "删除";
  if (entry.kind === "unmerged" || entry.xy.includes("U")) return "冲突";
  if (entry.kind === "type-changed" || entry.xy.includes("T")) return "类型变更";
  if (entry.kind === "modified" || entry.xy.includes("M")) return "修改";
  return "变更";
}

/** Format byte sizes compactly for the row meta line. */
export function formatBytes(size: number | null | undefined): string {
  if (size == null) return "—";
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

/** Render an RFC3339 timestamp as a local, second-precision 采集时间 string. */
export function formatCollectedAt(value: string | null | undefined): string {
  if (!value) return "—";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return parsed.toLocaleString("zh-CN", { hour12: false });
}
