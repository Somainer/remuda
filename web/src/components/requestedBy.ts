import type { RequestedBy } from "../types/generated";

/**
 * D-060 companion item (cb-requested-by): who asked for an Agent-created
 * dispatch or task, as the Hub recorded it from its own root turn.
 *
 * Provenance only. The Hub records `requestedBy` and never gates on it: the
 * approval decision and the admission rules are the same with or without it.
 *
 * - The stored record is a reference (`commandId`, or a non-owner `origin`).
 *   For the owner's own reads of the approval cards, the board and a task
 *   detail, the Hub adds a read-time `excerpt` of the owner's words from its
 *   command row; it is not part of the generated type, so it is read here as
 *   untrusted data.
 * - Any other origin is labelled "agent 自发" (owner ruling R2). `/approvals`
 *   already carries `RootTurnTag` for that label; surfaces without a
 *   `rootTurn` of their own, such as a task document, use `RequestedByTag`.
 */

/** The owner's words, trimmed; null when the read carried none. */
export function requestedByExcerpt(
  requestedBy?: RequestedBy | null,
): string | null {
  const excerpt = requestedBy?.excerpt;
  if (typeof excerpt !== "string") return null;
  const trimmed = excerpt.trim();
  return trimmed ? trimmed : null;
}

/** The non-owner label for `requestedBy`, or null for an owner turn. */
export function requestedByTagText(
  requestedBy?: RequestedBy | null,
): string | null {
  const origin = requestedBy?.origin;
  if (!origin || origin === "human") return null;
  return "agent 自发";
}

/** Hover text for the non-owner label: who started the turn. */
export function requestedByTitle(requestedBy: RequestedBy): string {
  const source = requestedBy.origin ?? "unknown";
  return `这一回合不是由你发起的：来源 ${source}。仅作记录，不影响授权。`;
}
