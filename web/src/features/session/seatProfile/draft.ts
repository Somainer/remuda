/** The seat-profile editor's draft: the fields as text, and the PUT body. */
import type { SeatProfile, SeatProfileUpdate } from "./api";

/** The editor's fields, as text. */
export type Draft = {
  displayName: string;
  role: string;
  models: string;
  effort: string;
  permissionMode: string;
  skills: string;
};

export function draftOf(profile: SeatProfile | null | undefined, fallbackMode: string): Draft {
  return {
    displayName: profile?.displayName ?? "",
    role: profile?.role ?? "",
    models: (profile?.modelCandidates ?? []).join("\n"),
    effort: (profile?.effort as { name?: string } | null | undefined)?.name ?? "",
    permissionMode: profile?.permissionMode ?? fallbackMode,
    skills: (profile?.skills ?? []).join("\n"),
  };
}

function lines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/**
 * Only the fields the owner changed: an untouched field keeps the Hub's value
 * (on a first edit, the live chapter's spec), so opening the editor and
 * saving never rewrites a mode or a model the panel only guessed.
 */
export function bodyOf(draft: Draft, initial: Draft, expectedRevision: number): SeatProfileUpdate {
  const body: SeatProfileUpdate = { expectedRevision };
  if (draft.displayName !== initial.displayName) body.displayName = draft.displayName.trim() || null;
  if (draft.role !== initial.role) body.role = draft.role.trim() || null;
  if (draft.models !== initial.models) body.modelCandidates = lines(draft.models);
  if (draft.effort !== initial.effort) body.effort = draft.effort ? { name: draft.effort, ultracode: false } : null;
  if (draft.permissionMode !== initial.permissionMode) body.permissionMode = draft.permissionMode;
  if (draft.skills !== initial.skills) body.skills = lines(draft.skills);
  return body;
}
