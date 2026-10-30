/**
 * The Hub's D-060 memory routes (protocol.md §8.4) for the PWA memory page.
 *
 * Every write carries `expectedRevision`; the caller renders a 409
 * MEMORY_REVISION_CONFLICT as a choice, never as an overwrite. The client
 * only addresses a lineage, so the page resolves the route instance's
 * lineage first.
 */
import { rest } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import type { components } from "../../lib/api.generated";

export type MemoryEntry = components["schemas"]["MemoryEntry"];
export type MemorySearchHit = components["schemas"]["MemorySearchHit"];
export type MemoryWriteResult = components["schemas"]["MemoryWriteResult"];
export type MemoryWriteItem = components["schemas"]["MemoryWriteItem"];
export type MemoryWritePage = components["schemas"]["MemoryWritePage"];
export type MemoryType = components["schemas"]["MemoryType"];
export type MemorySizeWarning = components["schemas"]["MemorySizeWarning"];

/** The four type groups, in index order (user, feedback, project, reference). */
export const MEMORY_TYPES: readonly MemoryType[] = ["user", "feedback", "project", "reference"];

/** Chinese group labels for the page; the wire values stay the keys. */
export const MEMORY_TYPE_LABEL: Record<MemoryType, string> = {
  user: "用户",
  feedback: "反馈",
  project: "项目",
  reference: "参考",
};

export const ABOUT_ME_SLUG = "about-me";

/** Fields a create or an edit sends. */
export type MemoryDraft = {
  type: MemoryType;
  title: string;
  description: string;
  body: string;
};

function base(lineageId: string): string {
  return `/v1/lineages/${encodeURIComponent(lineageId)}/memory`;
}

/** Every entry of the lineage in index order. */
export async function listMemory(
  lineageId: string,
  includeArchived = true,
): Promise<MemoryEntry[]> {
  const page = await rest<{ items?: MemoryEntry[] }>(
    `${base(lineageId)}?includeArchived=${includeArchived ? "true" : "false"}`,
  );
  return Array.isArray(page?.items) ? page.items : [];
}

/**
 * FTS5 first, LIKE when the tokenizer cannot see the query (a CJK run is one
 * token), so a short Chinese query still finds its entry.
 */
export async function searchMemory(
  lineageId: string,
  q: string,
  includeArchived = true,
): Promise<MemoryEntry[]> {
  const params = new URLSearchParams({
    q,
    limit: "100",
    includeArchived: includeArchived ? "true" : "false",
  });
  const page = await rest<{ items?: MemorySearchHit[] }>(`${base(lineageId)}/search?${params}`);
  return (Array.isArray(page?.items) ? page.items : []).map((hit) => hit.entry);
}

/** Whether the Hub has a lineage row for `lineageId`.
 *
 * An unknown lineage answers `200 {items: []}` on the list route, so the page
 * decides existence explicitly. A retired lineage also has no row, but its
 * archived entries are still listed (and restorable) by the memory routes.
 */
export async function lineageExists(lineageId: string): Promise<boolean> {
  try {
    await rest(`/v1/lineages/${encodeURIComponent(lineageId)}`);
    return true;
  } catch (error) {
    if (error instanceof HubHttpError && error.status === 404) return false;
    throw error;
  }
}

export async function getMemory(lineageId: string, slug: string): Promise<MemoryEntry> {
  return rest<MemoryEntry>(`${base(lineageId)}/entries/${encodeURIComponent(slug)}`);
}

export async function createMemory(
  lineageId: string,
  draft: MemoryDraft & { slug: string },
): Promise<MemoryWriteResult> {
  return rest<MemoryWriteResult>(base(lineageId), {
    method: "POST",
    body: JSON.stringify(draft),
  });
}

export async function updateMemory(
  lineageId: string,
  slug: string,
  expectedRevision: number,
  patch: Partial<MemoryDraft>,
): Promise<MemoryWriteResult> {
  return rest<MemoryWriteResult>(`${base(lineageId)}/entries/${encodeURIComponent(slug)}`, {
    method: "PUT",
    body: JSON.stringify({ ...patch, expectedRevision }),
  });
}

export async function deleteMemory(
  lineageId: string,
  slug: string,
  expectedRevision: number,
): Promise<void> {
  await rest<unknown>(
    `${base(lineageId)}/entries/${encodeURIComponent(slug)}?expectedRevision=${expectedRevision}`,
    { method: "DELETE" },
  );
}

/** Owner restore of archived entries. */
export async function restoreMemory(lineageId: string, slugs: string[]): Promise<string[]> {
  const page = await rest<{ restored?: string[] }>(`${base(lineageId)}/restore`, {
    method: "POST",
    body: JSON.stringify({ slugs }),
  });
  return Array.isArray(page?.restored) ? page.restored : [];
}

/**
 * One page of a writer chapter's append-only write log, oldest first within
 * the page. Without `afterId` the Hub returns the NEWEST page; pass the
 * previous page's `nextAfterId` to continue. Pages are keyed by log row id, so
 * a turn with more rows than a page still advances.
 */
export async function listMemoryWrites(
  instanceId: string,
  afterId?: number,
  signal?: AbortSignal,
): Promise<MemoryWritePage> {
  const query = afterId === undefined ? "" : `?afterId=${afterId}`;
  const page = await rest<Partial<MemoryWritePage>>(
    `/v1/instances/${encodeURIComponent(instanceId)}/memory-writes${query}`,
    signal ? { signal } : {},
  );
  return {
    items: Array.isArray(page?.items) ? page.items : [],
    nextAfterId: typeof page?.nextAfterId === "number" ? page.nextAfterId : 0,
  };
}
