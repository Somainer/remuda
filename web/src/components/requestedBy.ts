import type { RequestedBy } from "../types/generated";

/**
 * D-060 companion item (cb-requested-by): who asked for an Agent-created
 * dispatch or task, as the Hub recorded it from its own root turn.
 *
 * Provenance only. The Hub records `requestedBy` and never gates on it: the
 * approval decision and the admission rules are the same with or without it.
 *
 * - A request made in an owner turn carries the owner's own words.
 * - Any other origin is labelled "agent 自发" (owner ruling R2). `/approvals`
 *   already carries `RootTurnTag` for that label; surfaces without a
 *   `rootTurn` of their own, such as a task document, use `RequestedByTag`.
 */

/** The owner's words, trimmed; null when none were recorded. */
export function requestedByExcerpt(
  requestedBy?: RequestedBy | null,
): string | null {
  const excerpt = requestedBy?.excerpt?.trim();
  return excerpt ? excerpt : null;
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
