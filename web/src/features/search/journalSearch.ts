/**
 * cb-search: `GET /v1/search` (protocol.md §8.4) for the session list's
 * record search. The Hub scopes the hits to what this device may read; the
 * snippet is the row's redacted text, untrusted data rendered as plain text.
 */
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";

export type JournalSearchHit = components["schemas"]["JournalSearchHit"];
export type JournalSearchResult = components["schemas"]["JournalSearchResult"];

/** Hits one search asks for: the route's cap. */
export const JOURNAL_SEARCH_LIMIT = 50;

export async function searchJournal(q: string, signal?: AbortSignal): Promise<JournalSearchResult> {
  const params = new URLSearchParams({ q, limit: String(JOURNAL_SEARCH_LIMIT) });
  const page = await rest<Partial<JournalSearchResult>>(`/v1/search?${params}`, {}, signal);
  return {
    hits: Array.isArray(page?.hits) ? page.hits : [],
    partial: page?.partial === true,
    indexComplete: page?.indexComplete !== false,
  };
}

/** Where a hit opens: the session's events tab at that row. */
export function hitHref(hit: Pick<JournalSearchHit, "instanceId" | "seq">): string {
  return `/s/${encodeURIComponent(hit.instanceId)}/events?seq=${encodeURIComponent(hit.seq)}`;
}

const KIND_LABEL: Record<string, string> = {
  message: "消息",
  tool_call: "工具调用",
  tool_result: "工具结果",
};

const ROLE_LABEL: Record<string, string> = {
  user: "用户",
  assistant: "助手",
};

/** 「助手消息」, 「工具结果」…: the row's kind in the list's words. */
export function hitKindLabel(hit: Pick<JournalSearchHit, "kind" | "role">): string {
  const kind = KIND_LABEL[hit.kind] ?? hit.kind;
  const role = hit.role ? ROLE_LABEL[hit.role] : undefined;
  return role ? `${role}${kind}` : kind;
}
