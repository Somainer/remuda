import type { DeviceSession } from "./session";

/**
 * New-session drafts (UX batch B, exploration §P0-2.4).
 *
 * The Composer keeps its per-instance drafts under `runtime.draft.<instanceId>`
 * (see `drafts.ts`). The New Session page has no instance yet, so its drafts
 * live under a separate, versioned namespace:
 *
 *   runtime.draft.new.v1.<authSubject>.<hostId>.<workspaceId>
 *
 * Three rules make this safe:
 *
 * 1. Without a reliable authenticated subject the draft never touches
 *    localStorage — it stays in a module-level memory map (the page keeps a
 *    `useRef` mirror too) and dies with the tab. A shared computer must not
 *    leak one person's prompt into another person's browser profile.
 * 2. Keying includes the authenticated subject, the host and the workspace, so
 *    switching account or directory can never surface another context's draft.
 * 3. Only the prompt body and a fixed allowlist of non-sensitive options are
 *    persisted. Auth values, settings-overlay/config paths, budgets, custom
 *    executables, extra argv, temp-file contents and attachment bytes are not
 *    part of the draft shape and are stripped even if a caller tries to pass
 *    them.
 *
 * This module never reads or writes the legacy `runtime.draft.<instanceId>`
 * namespace; rollback of batch B leaves Composer drafts intact.
 */

const PREFIX = "runtime.draft.new.v1.";
/** Bumped only with an explicit migration; today there is just v1. */
const DRAFT_VERSION = 1 as const;
/** Prompts are user text; cap storage so a paste can't exhaust quota. */
const MAX_PROMPT_CHARS = 100_000;
const MAX_SHORT_CHARS = 4_096;

/** The non-sensitive options a New Session draft is allowed to carry. */
export type NewSessionDraft = {
  prompt: string;
  kind?: string;
  model?: string;
  permissionMode?: string;
  delegation?: string;
  cwdMode?: "existing" | "worktree";
  cwdPath?: string;
  worktreeName?: string;
  effortKind?: string;
  effortIndex?: number;
  effortUltracode?: boolean;
};

type StoredDraft = NewSessionDraft & { v: typeof DRAFT_VERSION };

/**
 * Memory-only drafts, keyed without an auth subject: `${hostId}␀${workspaceId}`.
 * Module scope on purpose — one browser context, one tab session; never
 * serialized.
 */
const memoryDrafts = new Map<string, NewSessionDraft>();

function memoryKey(hostId: string, workspaceId: string): string {
  return `${hostId}␀${workspaceId}`;
}

/** localStorage key segments are restricted so ids can't forge namespaces. */
function keyPart(value: string): string {
  return value.replace(/[^A-Za-z0-9._-]/g, "_");
}

/** Build the persistence key. Exported for tests and for callers debugging. */
export function newSessionDraftKey(authSubject: string, hostId: string, workspaceId: string): string {
  return `${PREFIX}${keyPart(authSubject)}.${keyPart(hostId)}.${keyPart(workspaceId)}`;
}

/**
 * The reliable auth subject for draft isolation: the Hub-issued device id of
 * the current device session. Anything else (logged out, mock marker that
 * never authenticated) means "no reliable subject" → memory-only drafts.
 */
export function draftAuthSubject(session: DeviceSession | null | undefined): string | null {
  const deviceId = session?.deviceId;
  return typeof deviceId === "string" && deviceId.trim() ? deviceId : null;
}

function asString(value: unknown, max: number): string | undefined {
  if (typeof value !== "string") return undefined;
  const trimmed = value.slice(0, max);
  return trimmed || undefined;
}

function asOptionString(value: unknown, max: number): string | undefined {
  const s = asString(value, max);
  return s == null ? undefined : s;
}

/**
 * Rebuild a draft from untrusted JSON through the allowlist. Unknown fields —
 * including anything that looks like a credential or a path to secrets — are
 * dropped here, not just omitted at write time.
 */
export function sanitizeDraft(raw: unknown): NewSessionDraft | null {
  if (raw === null || typeof raw !== "object") return null;
  const record = raw as Record<string, unknown>;
  if (record.v !== DRAFT_VERSION) return null;
  if (typeof record.prompt !== "string") return null;
  const cwdMode = record.cwdMode === "worktree" ? "worktree" : record.cwdMode === "existing" ? "existing" : undefined;
  const effortIndex =
    typeof record.effortIndex === "number" && Number.isFinite(record.effortIndex)
      ? Math.min(20, Math.max(0, Math.trunc(record.effortIndex)))
      : undefined;
  const kind = asOptionString(record.kind, 64);
  const model = asOptionString(record.model, 200);
  const permissionMode = asOptionString(record.permissionMode, 64);
  const delegation = asOptionString(record.delegation, 64);
  const cwdPath = asString(record.cwdPath, MAX_SHORT_CHARS);
  const worktreeName = asString(record.worktreeName, MAX_SHORT_CHARS);
  const effortKind = asOptionString(record.effortKind, 64);
  const draft: NewSessionDraft = {
    prompt: record.prompt.slice(0, MAX_PROMPT_CHARS),
    ...(kind ? { kind } : {}),
    ...(model ? { model } : {}),
    ...(permissionMode ? { permissionMode } : {}),
    ...(delegation ? { delegation } : {}),
    ...(cwdMode ? { cwdMode } : {}),
    ...(cwdPath ? { cwdPath } : {}),
    ...(worktreeName ? { worktreeName } : {}),
    ...(effortKind ? { effortKind } : {}),
    ...(effortIndex != null ? { effortIndex } : {}),
    ...(typeof record.effortUltracode === "boolean" ? { effortUltracode: record.effortUltracode } : {}),
  };
  return draft;
}

/** A draft with nothing in its body or directory fields is not worth keeping. */
export function isEmptyDraft(draft: NewSessionDraft): boolean {
  return !draft.prompt.trim() && !draft.cwdPath?.trim() && !draft.worktreeName?.trim();
}

/** Project caller state through the same allowlist before serializing. */
function toStored(draft: NewSessionDraft): StoredDraft {
  return { v: DRAFT_VERSION, ...sanitizeDraft({ v: DRAFT_VERSION, ...draft })! };
}

/**
 * Persist the draft for one (subject, host, workspace) context.
 *
 * Pass `authSubject: null` when no reliable identity exists: the draft is held
 * in memory only and nothing is written to localStorage. An empty draft removes
 * any previously stored one.
 */
export function saveNewSessionDraft(
  authSubject: string | null,
  hostId: string,
  workspaceId: string,
  draft: NewSessionDraft,
): { persisted: boolean } {
  const clean = sanitizeDraft({ v: DRAFT_VERSION, ...draft });
  if (!clean || isEmptyDraft(clean)) {
    clearNewSessionDraft(authSubject, hostId, workspaceId);
    return { persisted: Boolean(authSubject) };
  }
  if (!authSubject) {
    memoryDrafts.set(memoryKey(hostId, workspaceId), clean);
    return { persisted: false };
  }
  memoryDrafts.delete(memoryKey(hostId, workspaceId));
  try {
    localStorage.setItem(newSessionDraftKey(authSubject, hostId, workspaceId), JSON.stringify(toStored(clean)));
    return { persisted: true };
  } catch {
    /* quota / disabled storage: fall back to memory rather than losing input */
    memoryDrafts.set(memoryKey(hostId, workspaceId), clean);
    return { persisted: false };
  }
}

/** Read the draft for one context; `null` when none exists or data is corrupt. */
export function loadNewSessionDraft(
  authSubject: string | null,
  hostId: string,
  workspaceId: string,
): NewSessionDraft | null {
  if (!authSubject) return memoryDrafts.get(memoryKey(hostId, workspaceId)) ?? null;
  try {
    const raw = localStorage.getItem(newSessionDraftKey(authSubject, hostId, workspaceId));
    if (!raw) return null;
    return sanitizeDraft(JSON.parse(raw) as unknown);
  } catch {
    return null;
  }
}

/** Remove the draft for one context (explicit discard or successful create). */
export function clearNewSessionDraft(authSubject: string | null, hostId: string, workspaceId: string): void {
  memoryDrafts.delete(memoryKey(hostId, workspaceId));
  if (!authSubject) return;
  try {
    localStorage.removeItem(newSessionDraftKey(authSubject, hostId, workspaceId));
  } catch {
    /* storage unavailable: nothing persisted */
  }
}

/** Test helper: how many memory-only drafts the current tab holds. */
export function memoryDraftCountForTest(): number {
  return memoryDrafts.size;
}
