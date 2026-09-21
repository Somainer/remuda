//! Task space vs project space — the client-side filter projection over the
//! file view that already exists (plan task-model task 7 t-taskspace;
//! D-050 §7; docs/design/task-model.md §7; files-view-contract.md §3).
//!
//! There is deliberately NO new Node or Hub endpoint here. Both spaces read
//! the same live `workspace.scm.status` payload (`filesApi.ts`):
//!  - the project space is the unchanged worktree tree on its
//!    `hostId + workspaceId` axis;
//!  - the task space is that same payload narrowed to the task's scope.
//!
//! Scope has two parts, matching the design wording ("task 的 session 集合 +
//! owns[] glob"):
//!  - the task's session set (`instances.task_id`, surfaced by
//!    `GET /v1/tasks/{id}/placements`) fixes the workspace axis — the panel is
//!    only ever mounted over a workspace the task's sessions ran in;
//!  - the `owns[]` globs filter rows. The SCM payload carries no per-session
//!    attribution (files-view-contract.md §2.1 forbids one), so path
//!    narrowing can only come from the declared ownership globs. A task that
//!    declared no owns therefore projects to the empty state — we never
//!    synthesise entries to fill it.
//!
//! Glob semantics mirror `remuda_protocol::glob_match`/`normalize_glob`
//! (crates/remuda-protocol/src/task.rs) byte for byte: `?` is one non-`/`
//! byte, `*` stays inside one path segment, `**` crosses `/`.

import type { ScmEntry, ScmStatus } from "../../types/scm";
import type { Task } from "../../types/generated";

/**
 * The task-space projection scope. `sessionIds` is the task's session set
 * (instance ids from its placement ledger rows); `owns` are the raw globs
 * stored on the task.
 */
export interface TaskSpaceScope {
  taskId: string;
  sessionIds: readonly string[];
  owns: readonly string[];
}

/** Structural shape of one placement ledger row the scope builder reads. */
export type TaskPlacementRow = {
  instanceId?: string | null;
};

// ── Glob matching (TS port of remuda_protocol::glob_match) ─────────────────

const encoder = new TextEncoder();

/**
 * Normalise one ownership glob exactly like the Hub's `normalize_glob`:
 * trim, backslashes to slashes, drop repeated `./` and any leading `/`; a
 * trailing slash turns the pattern into the subtree glob (`foo/` → `foo/**`);
 * the empty pattern means everything (`**`).
 */
export function normalizeGlob(raw: string): string {
  let pattern = raw.trim().replaceAll("\\", "/");
  while (pattern.startsWith("./")) {
    pattern = pattern.slice(2);
  }
  pattern = pattern.replace(/^\/+/, "");
  if (pattern === "") return "**";
  if (pattern.endsWith("/")) {
    const dir = pattern.slice(0, -1);
    return dir === "" ? "**" : `${dir}/**`;
  }
  return pattern;
}

/** Normalise a claim-glob set, dropping nothing valid and de-duplicating. */
export function normalizeGlobs(patterns: readonly string[]): string[] {
  const out = patterns.map((raw) => normalizeGlob(raw)).filter((p) => p !== "");
  out.sort();
  return [...new Set(out)];
}

/**
 * Match a repo-relative path against an ownership glob. Direct port of the
 * recursive matcher in crates/remuda-protocol/src/task.rs (same `*`/`**`/`?`
 * rules, byte comparison so UTF-8 paths behave identically).
 */
export function globMatch(pattern: string, path: string): boolean {
  const p = encoder.encode(pattern);
  const s = encoder.encode(path);

  const rec = (pi: number, si: number): boolean => {
    if (pi >= p.length) return si >= s.length;
    const b = p[pi];
    if (b === 0x3f /* ? */) {
      return si < s.length && s[si] !== 0x2f /* / */ && rec(pi + 1, si + 1);
    }
    if (b === 0x2a /* * */) {
      if (p[pi + 1] === 0x2a /* ** */) {
        const after = pi + 2;
        if (after >= p.length) return true;
        if (p[after] === 0x2f /* / */) {
          const tail = after + 1;
          return rec(tail, si) || (si < s.length && rec(pi, si + 1));
        }
        // `**` away from a slash boundary behaves like a single `*`.
        return rec(after, si) || (si < s.length && s[si] !== 0x2f && rec(pi, si + 1));
      }
      return rec(pi + 1, si) || (si < s.length && s[si] !== 0x2f && rec(pi, si + 1));
    }
    return si < s.length && s[si] === b && rec(pi + 1, si + 1);
  };

  return rec(0, 0);
}

// ── Projection ─────────────────────────────────────────────────────────────

/**
 * Paths an entry occupies, normalised for matching. A rename carries both the
 * new path and `origPath`; either side inside owns keeps the row (the gate
 * owns the whole change).
 */
export function entryPaths(entry: Pick<ScmEntry, "path" | "origPath">): string[] {
  const paths = [normalizeGlob(entry.path)];
  if (entry.origPath) paths.push(normalizeGlob(entry.origPath));
  return paths;
}

/** True when one status row projects into the task space. */
export function entryInTaskSpace(
  entry: Pick<ScmEntry, "path" | "origPath">,
  scope: Pick<TaskSpaceScope, "owns">,
): boolean {
  const globs = normalizeGlobs(scope.owns);
  if (globs.length === 0) return false;
  return entryPaths(entry).some((path) => globs.some((glob) => globMatch(glob, path)));
}

/**
 * Project status rows onto the task space. Returns a new array; the input
 * payload is never mutated. Empty owns (or no match) → `[]`, the caller's
 * 「还没有文件」 state — never a synthesised row.
 */
export function filterTaskEntries(
  entries: readonly ScmEntry[],
  scope: Pick<TaskSpaceScope, "owns">,
): ScmEntry[] {
  const globs = normalizeGlobs(scope.owns);
  if (globs.length === 0) return [];
  return entries.filter((entry) =>
    entryPaths(entry).some((path) => globs.some((glob) => globMatch(glob, path))),
  );
}

/**
 * Availability pass-through boundary. Returns `null` for every non-`changes`
 * view phase (loading / clean / unsupported / forbidden / offline / missing /
 * failed / not-collected): the six files-view-contract §3.6 states render
 * exactly as the project space renders them, with no fabricated baseline.
 * Only a resolved `changes` view projects — to a possibly empty row set.
 */
export function taskSpaceEntries(
  view: { phase: string; status: ScmStatus | null },
  scope: Pick<TaskSpaceScope, "owns">,
): ScmEntry[] | null {
  if (view.phase !== "changes" || !view.status) return null;
  return filterTaskEntries(view.status.entries, scope);
}

// ── Scope builders ─────────────────────────────────────────────────────────

/**
 * Build the task-space scope from a task document and its placement ledger
 * rows (`GET /v1/tasks/{id}/placements`). The placement rows' instance ids
 * are the task's session set (`instances.task_id`).
 */
export function taskScopeFrom(
  task: Pick<Task, "id" | "owns">,
  placements: readonly TaskPlacementRow[] = [],
): TaskSpaceScope {
  const sessionIds = [
    ...new Set(
      placements
        .map((row) => row.instanceId)
        .filter((id): id is string => typeof id === "string" && id.length > 0),
    ),
  ];
  return { taskId: task.id, sessionIds, owns: task.owns ?? [] };
}

/** Whether the task's session set includes the given instance. */
export function taskHasSession(scope: TaskSpaceScope, instanceId: string): boolean {
  return scope.sessionIds.includes(instanceId);
}

/**
 * Whether the task's recorded directory binding targets this panel's
 * `hostId + workspaceId` axis. The instance match alone is not enough: a
 * task can hold a placement for the session while its `workspaceBinding`
 * names another workspace (moved/rebound tasks). Labelling this panel with
 * that task would apply a foreign owns[] set to unrelated rows, so the
 * caller must treat such a match as "no owning task here".
 *
 * A task without a stored binding cannot contradict the axis (its placement
 * rows still record the host they ran on); only a binding that positively
 * names a different host or workspace rejects it.
 */
export function taskServesAxis(
  task: Pick<Task, "workspaceBinding">,
  axis: { hostId: string; workspaceId: string },
): boolean {
  const binding = task.workspaceBinding;
  if (!binding) return true;
  return binding.hostId === axis.hostId && binding.workspaceId === axis.workspaceId;
}
