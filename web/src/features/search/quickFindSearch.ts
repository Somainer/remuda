import { projectStatus } from "../../lib/status";
import type { Instance, UiStatus } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import { OTHER_SPACE, spaceKey, type Space } from "../spaces/store";

/**
 * Cross-Space QuickFind ranking (exploration §5 P1-1).
 *
 * Pure ranking over the lists the Hub has **already** loaded into the client —
 * no remote index is consulted, and nothing here implies the full history is
 * searchable. When the connection to the Hub is down the same cache is all the
 * caller has, and {@link rankQuickFind} says so via `cacheOnly` so the UI can
 * label the scope rather than silently presenting stale rows as complete.
 *
 * Match priority is fixed by the spec:
 *
 * 1. session title
 * 2. Space name
 * 3. host name
 * 4. instance id — a *secondary* hit, never enough to outrank a title match
 *
 * Within one field, an exact match beats a prefix, a prefix beats a word start,
 * and a word start beats a plain substring. Ties break on most recent activity
 * so two equally-good hits keep a stable, useful order.
 */

export type QuickFindField = "title" | "space" | "host" | "id";

/** The Space shape the ranker needs; structurally compatible with `Space`. */
export type QuickFindSpace = Pick<Space, "id" | "name" | "hostId" | "instances">;

export type QuickFindHit = {
  instance: Instance;
  spaceId: string;
  spaceName: string;
  hostName: string;
  title: string;
  status: UiStatus;
  /** Which field matched; `null` for the unfiltered recency list. */
  field: QuickFindField | null;
  /**
   * Another candidate carries the same title. The row must then spell out
   * Space and host — a bare title would be ambiguous (exploration §5 P1-1
   * acceptance: same-name sessions are distinguished by Space + host).
   */
  ambiguousTitle: boolean;
};

export type QuickFindResult = {
  hits: QuickFindHit[];
  /** Number of hits before {@link QuickFindInput.limit}. */
  total: number;
  /** The Hub is unavailable; these rows come only from the local cache. */
  cacheOnly: boolean;
};

export type QuickFindInput = {
  spaces: QuickFindSpace[];
  query: string;
  titleOf: (instanceId: string) => string;
  hostNameOf: (hostId?: string) => string;
  /** Hub connection; anything but `live` makes the result cache-only. */
  connection?: "live" | "reconnecting" | "offline";
  /** Upper bound on returned hits; the full count still comes back as `total`. */
  limit?: number;
};

export const DEFAULT_LIMIT = 50;

/** Match quality inside one field; lower is better. -1 means no match. */
const QUALITY_EXACT = 0;
const QUALITY_PREFIX = 1;
const QUALITY_WORD_START = 2;
const QUALITY_CONTAINS = 3;
const QUALITY_NONE = -1;
type Quality = number;

function qualityOf(haystack: string, needle: string): Quality {
  if (!needle || haystack.length < needle.length) return QUALITY_NONE;
  if (haystack === needle) return QUALITY_EXACT;
  if (haystack.startsWith(needle)) return QUALITY_PREFIX;
  // A word boundary hit ("pay" in "payments api") ranks above a mid-word hit.
  const at = haystack.indexOf(needle, 1);
  if (at > 0 && /[\s\-_./·]/.test(haystack[at - 1] ?? "")) return QUALITY_WORD_START;
  if (haystack.includes(needle)) return QUALITY_CONTAINS;
  return QUALITY_NONE;
}

/** Field tiers, kept far enough apart that any title hit outranks any Space hit. */
const TIER: Record<QuickFindField, number> = { title: 0, space: 10, host: 20, id: 30 };

type Candidate = QuickFindHit & { score: number };

function recency(hit: QuickFindHit): number {
  // Timestamp strings sort lexicographically; negate via a padded numeric key.
  return Date.parse(hit.instance.updatedAt) || 0;
}

/**
 * Rank the cached instances against `query`.
 *
 * With an empty query it returns every cached instance by recency (the panel
 * opens showing what is available); with a query it returns only instances
 * where the title, Space, host or id matches, in the fixed priority order.
 */
export function rankQuickFind(input: QuickFindInput): QuickFindResult {
  const needle = input.query.trim().toLowerCase();
  const titleOf = input.titleOf;
  const hostNameOf = input.hostNameOf;

  const hits = input.spaces.flatMap((space) =>
    space.instances.map((instance) => ({
      instance,
      spaceId: space.id,
      spaceName: space.name,
      hostName: hostNameOf(space.hostId ?? instance.hostId) || instance.hostId,
      title: titleOf(instance.id) || "会话",
      status: projectStatus(instance),
      field: null as QuickFindField | null,
      ambiguousTitle: false,
    })),
  );

  // A title appearing on more than one instance is ambiguous everywhere, so
  // both rows qualify with Space + host rather than only the "second" one.
  const titleCounts = new Map<string, number>();
  for (const hit of hits) {
    const key = hit.title.trim().toLowerCase();
    if (key) titleCounts.set(key, (titleCounts.get(key) ?? 0) + 1);
  }
  for (const hit of hits) hit.ambiguousTitle = (titleCounts.get(hit.title.trim().toLowerCase()) ?? 0) > 1;

  let candidates: Candidate[];
  if (!needle) {
    candidates = hits.map((hit) => ({ ...hit, score: 0 }));
    candidates.sort((a, b) => recency(b) - recency(a) || a.instance.id.localeCompare(b.instance.id));
  } else {
    candidates = [];
    for (const hit of hits) {
      const checks: Array<[QuickFindField, Quality]> = [
        ["title", qualityOf(hit.title.toLowerCase(), needle)],
        ["space", qualityOf(hit.spaceName.toLowerCase(), needle)],
        ["host", qualityOf(hit.hostName.toLowerCase(), needle)],
        // IDs are long opaque strings: exact/prefix make no sense there, so an
        // id contributes a plain substring hit at the lowest tier only.
        ["id", hit.instance.id.toLowerCase().includes(needle) ? QUALITY_CONTAINS : QUALITY_NONE],
      ];
      let field: QuickFindField | null = null;
      let best = Number.MAX_SAFE_INTEGER;
      for (const [candidateField, quality] of checks) {
        if (quality < 0) continue;
        const score = TIER[candidateField] + quality;
        if (score < best) {
          best = score;
          field = candidateField;
        }
      }
      if (field) candidates.push({ ...hit, field, score: best });
    }
    candidates.sort((a, b) => a.score - b.score || recency(b) - recency(a) || a.instance.id.localeCompare(b.instance.id));
  }

  const total = candidates.length;
  const limit = input.limit ?? DEFAULT_LIMIT;
  return {
    hits: candidates.slice(0, limit),
    total,
    cacheOnly: (input.connection ?? "live") !== "live",
  };
}

/**
 * Grouped Jump To (ui-spec §4.7 / D-049): the phone sheet groups the ranked
 * leaves by Space = (host, workspace). No second space model is built — the
 * groups are a view over {@link rankQuickFind}'s hits and `buildSpaces()`
 * data, and the leaves are always sessions (v1 has no pane hierarchy).
 */

export type QuickFindOrder = "clock" | "list";

/** Device-local (never cross-device) last Jump To ordering, ui-spec §1.4 style. */
export const QUICKFIND_ORDER_KEY = "remuda.mobile.quickfind.order.v1";

type OrderStorage = Pick<Storage, "getItem" | "setItem"> | null | undefined;

export function readQuickFindOrder(storage?: OrderStorage): QuickFindOrder {
  try {
    return storage?.getItem(QUICKFIND_ORDER_KEY) === "list" ? "list" : "clock";
  } catch {
    return "clock";
  }
}

export function writeQuickFindOrder(order: QuickFindOrder, storage?: OrderStorage): void {
  try {
    storage?.setItem(QUICKFIND_ORDER_KEY, order);
  } catch {
    /* The ordering is a convenience; never break the finder on storage failure. */
  }
}

export type QuickFindGroup = {
  /** Space id, the (hostId, workspaceId) key from `spaceKey()`. */
  id: string;
  project: string;
  hostName: string;
  /** Git branch of the project; null when the Node gave no branch. */
  branch: string | null;
  /** Always the buildSpaces() count, even while a search hides some leaves. */
  blockedCount: number;
  /** The sessions under this project — the pane layer is deliberately absent. */
  hits: QuickFindHit[];
};

/** The Space shape the grouper needs; structurally compatible with `Space`. */
export type QuickFindGroupSpace = Pick<Space, "id" | "hostId" | "workspaceId" | "blockedCount">;

/** The Workspace shape the grouper reads a branch from; structurally compatible. */
export type QuickFindBranchWorkspace = Pick<Workspace, "id" | "hostId" | "branch">;

function groupHitTime(hit: QuickFindHit): number {
  // The web Instance carries updatedAt as its status-change timestamp (the
  // same recency key rankQuickFind uses); there is no separate lastEventAt.
  return Date.parse(hit.instance.updatedAt) || 0;
}

/**
 * Group ranked hits for the phone Jump To sheet.
 *
 * The ordering rules mirror `homeRows.ts` `compareRows`/`compareGroups` (the
 * phone home's grouped list): the Other Space sorts last; blocked leaves pin
 * to the top of their group in every ordering (ui-spec §2.1 待处理置顶);
 * clock = most recent status change first, list = project name then title.
 * The rule is mirrored rather than imported so neither surface changes shape
 * for the other — keep the two in step.
 */
export function groupQuickFind(
  hits: QuickFindHit[],
  spaces: readonly QuickFindGroupSpace[],
  workspaces: readonly QuickFindBranchWorkspace[],
  order: QuickFindOrder,
): QuickFindGroup[] {
  const blockedById = new Map(spaces.map((space) => [space.id, space.blockedCount]));
  const branchBySpace = new Map<string, string>();
  for (const workspace of workspaces) {
    const branch = workspace.branch?.trim();
    if (branch) branchBySpace.set(spaceKey(workspace.hostId, workspace.id), branch);
  }

  const groups = new Map<string, QuickFindGroup>();
  for (const hit of hits) {
    let group = groups.get(hit.spaceId);
    if (!group) {
      const space = spaces.find((row) => row.id === hit.spaceId);
      const hostId = space?.hostId ?? hit.instance.hostId;
      const workspaceId = space?.workspaceId ?? hit.instance.workspaceId;
      const branchKey = hostId && workspaceId ? spaceKey(hostId, workspaceId) : hit.spaceId;
      group = {
        id: hit.spaceId,
        project: hit.spaceName,
        hostName: hit.hostName,
        branch: branchBySpace.get(branchKey) ?? null,
        blockedCount: blockedById.get(hit.spaceId) ?? 0,
        hits: [],
      };
      groups.set(hit.spaceId, group);
    }
    group.hits.push(hit);
  }

  for (const group of groups.values()) {
    group.hits.sort((a, b) => {
      const aBlocked = a.status === "blocked";
      const bBlocked = b.status === "blocked";
      if (aBlocked !== bBlocked) return aBlocked ? -1 : 1;
      if (order === "list") {
        return a.title.localeCompare(b.title) || a.instance.id.localeCompare(b.instance.id);
      }
      return groupHitTime(b) - groupHitTime(a) || a.instance.id.localeCompare(b.instance.id);
    });
  }

  return [...groups.values()].sort((a, b) => {
    if (a.id === OTHER_SPACE) return 1;
    if (b.id === OTHER_SPACE) return -1;
    if (order === "list") {
      return a.project.localeCompare(b.project) || a.id.localeCompare(b.id);
    }
    const latest = (candidate: QuickFindGroup) =>
      candidate.hits.reduce((max, hit) => Math.max(max, groupHitTime(hit)), 0);
    return latest(b) - latest(a) || a.id.localeCompare(b.id);
  });
}
