/**
 * Task directory binding selector — pure parsing for task creation
 * (plan task-model task 3 t-bind; D-050 §2; decision D2).
 *
 * The operator picks, per task:
 *  - `reuse` — an existing directory: the registered workspace root (no
 *    worktree name) or an existing `remuda-wt` sibling. No git operations;
 *    dispatch folds this into the existing `cwd` admission
 *    (`resolve_instance_cwd` on the Node).
 *  - `pool` — an app-managed worktree pool; the Hub leases a slot and the
 *    Node cuts a per-task branch `wt/<slot>/<task-slug>`.
 *
 * There is no silent default: the first task in a project forces an explicit
 * choice, then the choice is remembered per project (force-choose-then-
 * remember, D2). Everything here is pure and free of I/O except the
 * localStorage-backed memory.
 */
import type { components } from "../../lib/api.generated";

type TaskCreate = components["schemas"]["TaskCreate"];
type TaskSpaceBinding = components["schemas"]["TaskSpaceBinding"];
type WorktreeSharing = components["schemas"]["WorktreeSharing"];

/**
 * The wire binding accepted by `POST /v1/tasks`. It mirrors the stored
 * `TaskSpaceBinding` plus the request-only `base` (pool start-point
 * override); the Hub stores the resolved branch and lease row instead.
 */
export type BindingWire = Omit<TaskSpaceBinding, "branch" | "leaseRefIds"> & {
  base?: string;
};

/** Operator's choice in the selector. */
export type BindingChoice = {
  mode: "reuse" | "pool";
  hostId: string;
  workspaceId: string;
  /**
   * Reuse: omit/empty for the registered root; otherwise an existing
   * `remuda-wt` sibling name. Pool: the pool name (the Node allocates the
   * concrete `<pool>-s<n>` slot).
   */
  worktreeName?: string;
  /** Pool only: base branch override (defaults to the project base server-side). */
  base?: string;
};

/**
 * A safe worktree/pool segment mirrors the Node's `safe_segment`: lowercase
 * letter first, lowercase letters/digits/`_`/`-`, 1–32 chars. Anything that
 * escapes a single segment (`/`, `\`, `..`, a leading dot/dash, an absolute
 * path) is an out-of-tree target and rejected here exactly as
 * `resolve_instance_cwd` rejects it on the Node.
 */
const SAFE_SEGMENT = /^[a-z][a-z0-9_-]{0,31}$/;

/** The Node reserves the `<pool>-s<n>` suffix for allocated pool slots. */
const RESERVED_SLOT_SUFFIX = /-s\d+$/;

export type BindingError =
  | "mode-required"
  | "host-required"
  | "workspace-required"
  | "pool-name-required"
  | "out-of-tree"
  | "reserved-slot-suffix";

export type ParsedBinding =
  | { ok: true; binding: BindingWire }
  | { ok: false; error: BindingError };

function cleanName(name: string | undefined): string | undefined {
  const trimmed = name?.trim();
  return trimmed ? trimmed : undefined;
}

/**
 * True when `name` cannot be a contained `remuda-wt` sibling: path
 * separators, parent traversal, absolute paths, leading dots/dashes or any
 * shape `safe_segment` would refuse.
 */
export function isOutOfTree(name: string): boolean {
  return !SAFE_SEGMENT.test(name);
}

/** Parse and structurally validate the selector choice into a wire binding. */
export function parseBinding(choice: BindingChoice): ParsedBinding {
  if (choice.mode !== "reuse" && choice.mode !== "pool") {
    return { ok: false, error: "mode-required" };
  }
  if (!choice.hostId.trim()) return { ok: false, error: "host-required" };
  if (!choice.workspaceId.trim()) {
    return { ok: false, error: "workspace-required" };
  }
  const name = cleanName(choice.worktreeName);
  if (choice.mode === "pool") {
    if (!name) return { ok: false, error: "pool-name-required" };
    if (isOutOfTree(name)) return { ok: false, error: "out-of-tree" };
    if (RESERVED_SLOT_SUFFIX.test(name)) {
      return { ok: false, error: "reserved-slot-suffix" };
    }
  } else if (name !== undefined && isOutOfTree(name)) {
    return { ok: false, error: "out-of-tree" };
  }
  return {
    ok: true,
    binding: {
      mode: choice.mode,
      hostId: choice.hostId.trim(),
      workspaceId: choice.workspaceId.trim(),
      ...(name ? { worktreeName: name } : {}),
      ...(choice.mode === "pool" && choice.base?.trim()
        ? { base: choice.base.trim() }
        : {}),
    } as BindingWire,
  };
}

/**
 * Dispatch fold for a parsed binding (no new dispatch wire field, D-050 §3.4):
 *  - reuse root → no `cwd`; the Node applies `resolve_instance_cwd` to the
 *    registered root itself;
 *  - reuse sibling → `cwd` is the sibling's absolute path, resolved from the
 *    Node's `worktree.list` catalog by the caller;
 *  - pool → `cwd` is the leased slot path and `worktree` is the slot name,
 *    flowing through the existing CreateInstanceBody.worktree field.
 */
export type DispatchDirectory =
  | { kind: "root"; cwd: undefined; worktree: undefined }
  | { kind: "reuse"; cwd: string; worktree: undefined }
  | { kind: "pool"; cwd: string; worktree: string };

export function dispatchDirectory(
  binding: TaskSpaceBinding,
  resolvedPath: string | undefined,
): DispatchDirectory {
  if (binding.mode === "pool") {
    const name = binding.worktreeName;
    if (!name || !resolvedPath) {
      throw new Error("pool binding has no leased slot path");
    }
    return { kind: "pool", cwd: resolvedPath, worktree: name };
  }
  if (!binding.worktreeName) {
    return { kind: "root", cwd: undefined, worktree: undefined };
  }
  if (!resolvedPath) {
    throw new Error("reuse binding names a sibling but no catalog path resolved");
  }
  return { kind: "reuse", cwd: resolvedPath, worktree: undefined };
}

/**
 * "与 N 个 task 共用" — the serial-sharing footer for a lease row. `refcount`
 * is the number of tasks bound to one directory; 1 means this task alone.
 */
export function sharedWithTasksLabel(refcount: number): string | null {
  if (!Number.isFinite(refcount) || refcount < 2) return null;
  return `与 ${refcount} 个 task 共用`;
}

/** Whether a lease outcome means this task is queued behind an attached task. */
export function isQueuedSharing(sharing: Pick<WorktreeSharing, "refcount" | "queued">): boolean {
  return sharing.queued === true || sharing.refcount > 1;
}

// ── Per-project memory (D2: force-choose-then-remember) ────────────────────

const PREFIX = "runtime.task-binding.";

function key(projectId: string): string {
  return `${PREFIX}${projectId}`;
}

type RememberedChoice = { mode: "reuse" | "pool"; worktreeName?: string };

/** Remember the last binding choice for a project; drives the default UX. */
export function rememberBindingChoice(projectId: string, choice: RememberedChoice): void {
  try {
    localStorage.setItem(
      key(projectId),
      JSON.stringify({ mode: choice.mode, worktreeName: cleanName(choice.worktreeName) ?? null }),
    );
  } catch {
    /* storage unavailable: the first task simply re-asks */
  }
}

/** The remembered choice for a project, or null when the first task must ask. */
export function rememberedBindingChoice(projectId: string): RememberedChoice | null {
  try {
    const raw = localStorage.getItem(key(projectId));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<RememberedChoice>;
    if (parsed.mode !== "reuse" && parsed.mode !== "pool") return null;
    return {
      mode: parsed.mode,
      ...(parsed.worktreeName ? { worktreeName: parsed.worktreeName } : {}),
    };
  } catch {
    return null;
  }
}

export function hasRememberedBinding(projectId: string): boolean {
  return rememberedBindingChoice(projectId) !== null;
}

export function forgetBindingChoice(projectId: string): void {
  try {
    localStorage.removeItem(key(projectId));
  } catch {
    /* ignore */
  }
}

/**
 * Assemble a `/v1/tasks` create body from task fields plus a binding choice.
 * Throws on a structurally invalid choice; callers render the BindingError
 * instead of POSTing.
 */
export function buildTaskCreateBody(input: {
  projectId: string;
  title: string;
  intent: string;
  class?: TaskCreate["class"];
  owns?: string[];
  binding?: BindingChoice;
}): TaskCreate {
  const body: TaskCreate = {
    projectId: input.projectId,
    title: input.title,
    intent: input.intent,
    ...(input.class ? { class: input.class } : {}),
    ...(input.owns?.length ? { owns: input.owns } : {}),
  };
  if (input.binding) {
    const parsed = parseBinding(input.binding);
    if (!parsed.ok) {
      throw new Error(`invalid workspace binding: ${parsed.error}`);
    }
    body.workspaceBinding = parsed.binding;
  }
  return body;
}
