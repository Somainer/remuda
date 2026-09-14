//! Wire shapes for the Node SCM answers proxied at `/v1/.../changes`
//! (files-view-contract §3.5). Hand-kept to match the Node envelopes.

/** sha256 digest carried in the Node's file answers. */
export type Digest =
  | { state: "known"; value: string }
  | { state: "unknown"; reason: string };

/** One row of `workspace.scm.status`. */
export interface ScmEntry {
  path: string;
  origPath?: string | null;
  /** Classic `XY` code from `git status --porcelain` (space for the unchanged side). */
  xy: string;
  kind: string;
  sizeBytes?: number | null;
  oldOid?: string | null;
  newOid?: string | null;
  digest?: Digest;
}

export interface ScmLimits {
  maxEntries: number;
  maxDiffBytes: number;
  maxFileBytes: number;
}

/** `workspace.scm.status` envelope. */
export interface ScmStatus {
  workspaceId: string;
  root: string;
  scm: string;
  availability: "ok" | "unsupported" | "denied";
  unsupportedReason?: string;
  deniedReason?: string;
  headOid?: string | null;
  branch?: { state: "known" | "unknown"; value?: string; reason?: string };
  observedAt: string;
  entries: ScmEntry[];
  limits: ScmLimits;
  truncated: {
    entries: boolean;
    entriesOmitted: number;
    nonUtf8Omitted: number;
    statusBytes: boolean;
  };
  ignoreRules?: string;
}

/** One item of `workspace.scm.diff`. */
export interface ScmDiffItem {
  path: string;
  patch: string | null;
  binary: boolean;
  truncated: boolean;
  bytesAvailable: number;
}

/** `workspace.scm.diff` envelope. */
export interface ScmDiff {
  workspaceId?: string;
  availability: "ok" | "unsupported" | "denied";
  headOid?: string | null;
  staged: boolean;
  observedAt: string;
  items: ScmDiffItem[];
  truncated: { diffBytes: boolean; bytesOmitted: number };
}

/** `workspace.scm.file` envelope. */
export interface ScmFile {
  workspaceId?: string;
  availability: "ok" | "unsupported" | "denied";
  path: string;
  headOid?: string | null;
  observedAt: string;
  mediaType: string;
  binary: boolean;
  sizeBytes?: number | null;
  digest: Digest;
  content: string | null;
  truncated: boolean;
  limits?: ScmLimits;
}
