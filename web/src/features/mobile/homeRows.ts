import { formatListTime } from "../../lib/format";
import { projectStatus } from "../../lib/status";
import { endReason, type EndTone } from "../../lib/endReason";
import type { Activity, Instance, Kind, Lifecycle, UiStatus } from "../../types/instance";
import { knowledgeValue } from "../../types/command";
import type { Interaction } from "../../types/interaction";
import type { Observation } from "../../types/observation";
import type { Task } from "../../types/generated";
import type { UsageRollup } from "../session/contextUsage";
import { nextStep, type RowScreen } from "../session/nextStep";
import { OTHER_SPACE, type Space } from "../spaces/store";
import { rankQuickFind } from "../search/quickFindSearch";
import { buildTaskGroups, type TaskListGroup } from "../tasks/taskRows";

/**
 * Pure derivation for the `/m` phone home (ui-spec §4.7 / §2.1, D-038/D-049).
 *
 * It owns no state of its own: `buildSpaces()` groups the instances,
 * `nextStep()` produces the one sentence, `endReason()` owns an ended row's
 * human label and tone, `rankQuickFind()` owns the title/project/host search
 * boundary, and the Hub rollup owns the context percentage. This module only
 * decides grouping order, the live-error body rule and the row shape, so
 * every rule below is unit-testable without a DOM.
 *
 * UO-3 split the projection in two for the commit probe: `buildHomeRows()`
 * derives the per-instance display row (the part polling can change) and
 * `arrangeHomeGroups()` does the cheap query filter / sort / group shaping.
 * The phone list caches the derived rows by display signature, so a badge
 * flip (`interactions` changing without a visible row change) never commits
 * the list (`commit:HomeList`).
 */

export type HomeOrder = "clock" | "list";

/** Device-local (never cross-device) last home ordering, ui-spec §1.4 style. */
export const HOME_ORDER_KEY = "remuda.mobile.home.order.v1";

type OrderStorage = Pick<Storage, "getItem" | "setItem"> | null | undefined;

export function readHomeOrder(storage?: OrderStorage): HomeOrder {
  try {
    return storage?.getItem(HOME_ORDER_KEY) === "list" ? "list" : "clock";
  } catch {
    return "clock";
  }
}

export function writeHomeOrder(order: HomeOrder, storage?: OrderStorage): void {
  try {
    storage?.setItem(HOME_ORDER_KEY, order);
  } catch {
    /* The ordering is a convenience; never break the home on storage failure. */
  }
}

/**
 * The one ordering both `/m` and the desktop SessionList share (home.md §3.1).
 * Lower sorts first. `done-unseen` is a reserved slot: the per-owner `unseen`
 * flag does not exist until `seen-hub` lands, so nothing projects into it and
 * its position stays empty. `awaiting-wake` is a live idle session that still
 * has a child in the blocked, working or starting rank (a loop or a sub-agent
 * that will wake it); the future-wake half of that rule waits for `home-wakes`.
 */
export type HomeRank =
  | "blocked"
  | "done-unseen"
  | "working"
  | "awaiting-wake"
  | "starting"
  | "idle"
  | "exited";

/** Fixed order. Index is the sort key; never reorder these names. */
export const HOME_RANKS: readonly HomeRank[] = [
  "blocked",
  "done-unseen",
  "working",
  "awaiting-wake",
  "starting",
  "idle",
  "exited",
];

/**
 * Not yet a running process, or the Hub is still deciding what it is. A live
 * process is `ready` or `running`; those two fall through to the activity rows.
 */
const STARTING_LIFECYCLE: readonly Lifecycle[] = [
  "requested",
  "preparing",
  "starting",
  "unknown",
  "reconciling",
];
/** The process is gone or on its way out. */
const ENDED_LIFECYCLE: readonly Lifecycle[] = ["exited", "failed", "closing"];

/**
 * What the rank table reads off one instance. `childLive` is true when one of
 * this session's children itself ranks blocked, working or starting.
 */
export type RankInput = {
  lifecycle: Lifecycle;
  /** The known activity, or null when the Hub has not reported one. */
  activity: Activity | null;
  childLive?: boolean;
  /**
   * Per-owner "finished and not yet opened" flag. Absent until `seen-hub`
   * lands, so the done-unseen rank stays empty (home.md §3.1, §10).
   */
  unseen?: boolean;
  /** A settled root turn that failed. Such a turn is a queue card, never ◉. */
  lastTurnError?: boolean;
};

/**
 * The ordered first-match table (home.md §3.1). First hit wins, and the table
 * is total: after the starting and ended rows, only a live lifecycle remains,
 * so every `(lifecycle, activity)` pair lands on exactly one rank.
 *
 * Connectivity is deliberately NOT an input. A host that drops offline draws a
 * dashed dot and says 状态待确认, but the row keeps the rank its lifecycle and
 * activity give it — a blocked row does not fall out of rank 1.
 */
export function rankOf(input: RankInput): HomeRank {
  const { lifecycle, activity } = input;
  // a — waiting on a person, whatever the lifecycle.
  if (activity === "waiting-interaction") return "blocked";
  // b — not running yet, or the Hub has not settled the lifecycle.
  if (STARTING_LIFECYCLE.includes(lifecycle)) return "starting";
  // c — gone.
  if (ENDED_LIFECYCLE.includes(lifecycle)) return "exited";
  // d — a live turn in progress.
  if (activity === "working" || activity === "draining") return "working";
  if (activity === "idle") {
    // e — a successful turn the owner has not opened. A FAILED turn is a queue
    // card (§4.5), so it never takes the done-unseen rank.
    if (input.unseen === true && input.lastTurnError !== true) return "done-unseen";
    // f — idle, but a child is still going, or a wake is scheduled.
    if (input.childLive === true) return "awaiting-wake";
    // g — plain idle.
    return "idle";
  }
  // h — live, but the activity is unknown. Treated as working, not idle.
  return "working";
}

/**
 * Ranks a session from its own fields. `childLive` is the only extra fact, and
 * it only lifts an otherwise-idle live session into `awaiting-wake`.
 */
export function rank(instance: Instance, childLive = false): HomeRank {
  return rankOf({
    lifecycle: instance.lifecycle,
    activity: knowledgeValue(instance.activity) ?? null,
    childLive,
    // seen-hub: the owner has not opened the last settled turn. An older Hub
    // omits the flag, which reads as false.
    unseen: instance.unseen === true,
    lastTurnError: Boolean(instance.lastTurnError?.text?.trim()),
  });
}

/** Sort key for {@link rank}. `done-unseen` sits between blocked and working. */
export function rankOrder(homeRank: HomeRank): number {
  return HOME_RANKS.indexOf(homeRank);
}

/**
 * Status word on line 1. Only the three ranks whose section does not already
 * say the state get one: blocked, done-unseen (reserved, never reached today)
 * and awaiting-wake. Every other rank leaves the slot empty.
 */
export function rankStateWord(homeRank: HomeRank): string | null {
  if (homeRank === "blocked") return "待处理";
  if (homeRank === "done-unseen") return "完成未看";
  if (homeRank === "awaiting-wake") return "待唤醒";
  return null;
}

/**
 * A row that folds away inside a busy project. Only idle rows do: an exited
 * row belongs to the existing 已退出/已归档 fold (home.md §3.2), which this
 * surface does not have yet, so it stays visible rather than being hidden
 * behind an 「空闲」 label that would be wrong for it.
 */
export function rankFolds(homeRank: HomeRank): boolean {
  return homeRank === "idle";
}

/** A row a person may need to act on: ranks 1–5. Exited and idle are not. */
export function rankIsActive(homeRank: HomeRank): boolean {
  return homeRank !== "idle" && homeRank !== "exited";
}

export type HomeRow = {
  id: string;
  instance: Instance;
  title: string;
  status: UiStatus;
  /** The shared rank both surfaces sort by. */
  rank: HomeRank;
  /** Harness kind for the line-1 glyph (the existing HarnessGlyph). */
  kind: Kind;
  /** Line-1 state word; null unless the rank is blocked, done-unseen or awaiting-wake. */
  stateWord: string | null;
  /** The one body sentence: nextStep() verbatim — ended rows speak the shared
   *  endReason label, never the raw lastError (c-homeend). */
  body: string;
  bodyIsError: boolean;
  /**
   * EndTone for an ended row: `"failed"` is the ONLY tone the renderer paints
   * red; an interruption (Node restart) and an ordinary close stay muted. Null
   * for every live row (whose red, when any, is still `bodyIsError`).
   */
  bodyTone: EndTone | null;
  /**
   * Raw machine code behind an ended sentence, for the row tooltip only —
   * never visible text. Null for a live row or a clean exit.
   */
  bodyDetail: string | null;
  /** 0..100 used-context share; null = unknown, and the ring must not render. */
  contextPct: number | null;
  /** True when contextPct uses a kind-fallback window guess (render ≈). */
  contextPctApproximate: boolean;
  blocked: boolean;
  /** Exited rows advertise 恢复 only when the resume capability is really there (D-026). */
  canResume: boolean;
  /** 12px faint relative time for the row's right slot (formatListTime). */
  timeLabel: string;
  updatedAt: string;
};

export type HomeGroup = {
  id: string;
  project: string;
  hostName: string;
  /** Git branch of the project; null when the Node gave no branch. */
  branch: string | null;
  /** Always the buildSpaces() count, even while a search hides some rows. */
  blockedCount: number;
  /**
   * Header dot counts over every row the group holds, folded rows included.
   * Each rank keeps the glyph home.md §3.1 assigns it: ⚠ blocked, ◉ done-unseen,
   * ● working, ○ awaiting-wake and idle, a pulsing dot for starting, ■ exited.
   * A zero count is omitted by the renderer.
   */
  dots: {
    blocked: number;
    unseen: number;
    working: number;
    wake: number;
    starting: number;
    idle: number;
    exited: number;
  };
  /**
   * True when the group has no row in ranks 1–5 (no blocked, done-unseen,
   * working, awaiting-wake or starting session). Only idle and exited rows
   * remain, so the whole project folds into 「安静的项目」.
   */
  quiet: boolean;
  rows: HomeRow[];
};

export type HomeRowsInput = {
  spaces: Space[];
  interactions: Interaction[];
  order: HomeOrder;
  query: string;
  titleOf: (instanceId: string) => string;
  hostNameOf: (hostId?: string) => string;
  /** Live git branch per Space id; undefined/null = branch unknown. */
  branchOf?: (spaceId: string) => string | null | undefined;
  rollupOf: (instanceId: string) => UsageRollup | null;
  screenOf?: (instanceId: string) => RowScreen;
  summaryOf?: (instanceId: string) => string | undefined;
  eventsOf?: (instanceId: string) => Observation[] | undefined;
  /**
   * True when this session has a child that is still running. The only input
   * that lifts an idle row into the awaiting-wake rank; absent means no child.
   */
  childLiveOf?: (instanceId: string) => boolean;
  /** Clock injection for the relative-time label; defaults to Date.now(). */
  nowMs?: number;
};

/**
 * The body text of a RETRYABLE ROOT-TURN error on a LIVE row (D-057 OA6,
 * ma-sdk-state): the durable `instance.lastTurnError` marker the Hub sets on a
 * settled turn failure and clears on the next turn start. It is never a
 * process error — the instance stays running — so it is a separate marker from
 * [`homeError`] and does not survive a new turn.
 */
export function homeTurnError(instance: Instance): string | null {
  return instance.lastTurnError?.text?.trim() || null;
}

/**
 * The error text that owns the body slot of a LIVE row when the PROCESS (or
 * launch) failed. Ended rows never use this: endReason() owns their sentence
 * (c-homeend).
 *
 * `instance.lastError` is the Hub-merged process/start-failure channel. A
 * D-057 OA6 root-turn failure is deliberately NOT read here: it is emitted at
 * severity=info on `topic=turn` and surfaced via `homeTurnError` /
 * `instance.lastTurnError`, which the next turn start clears (a turn error
 * must not be a sticky process failure). The journal fallback mirrors the
 * process-error merge but skips turn/result frames and info-severity events.
 */
export function homeError(instance: Instance, events?: Observation[]): string | null {
  const direct = instance.lastError?.trim();
  if (direct) return direct;
  if (!events) return null;
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (event.kind !== "lifecycle") continue;
    const payload = event.payload as {
      type?: string;
      topic?: string;
      severity?: string;
      nativeName?: string;
      relatedIds?: { lastError?: unknown };
      status?: { value?: unknown };
    };
    if (payload?.type !== "native") continue;
    // Turn results are root-turn evidence (severity info), not process death.
    if (payload.topic === "turn") continue;
    // Only a genuine error-severity non-turn event is a process failure.
    if (payload.severity !== "error") continue;
    const name = String(payload.nativeName ?? "").toLowerCase();
    if (!name.includes("error")) continue;
    const text =
      typeof payload.relatedIds?.lastError === "string"
        ? payload.relatedIds.lastError
        : typeof payload.status?.value === "string"
          ? payload.status.value
          : "";
    const trimmed = text.trim();
    if (trimmed) return trimmed;
  }
  return null;
}

/**
 * Body-slot decision for a LIVE row. Unknown connectivity/lifecycle wins over
 * everything: the row reads 状态待确认 and an error can never turn it into a
 * positive phrase (D-038). On every other live status an error replaces the
 * next-step sentence verbatim. ENDED rows never reach here: their sentence is
 * the shared endReason() projection (c-homeend).
 */
export function homeBody(
  status: UiStatus,
  step: { text: string },
  error: string | null,
): { text: string; isError: boolean } {
  if (status === "unknown") return { text: step.text, isError: false };
  if (error) return { text: error, isError: true };
  return { text: step.text, isError: false };
}

/**
 * The full per-instance display derivation, keyed by instance id. One entry
 * per instance the spaces present (query filtering and sorting happen later
 * in `arrangeHomeGroups`); this is the slice the home list caches.
 */
export function buildHomeRows(
  input: Pick<
    HomeRowsInput,
    | "spaces"
    | "interactions"
    | "titleOf"
    | "rollupOf"
    | "screenOf"
    | "summaryOf"
    | "eventsOf"
    | "childLiveOf"
    | "nowMs"
  >,
): Map<string, HomeRow> {
  const nowMs = input.nowMs ?? Date.now();
  const rows = new Map<string, HomeRow>();
  for (const space of input.spaces) {
    for (const instance of space.instances) {
      if (rows.has(instance.id)) continue;
      const status = projectStatus(instance);
      const pending =
        input.interactions.find(
          (interaction) =>
            interaction.instanceId === instance.id && interaction.state === "pending",
        ) ?? null;
      const step = nextStep(
        instance,
        pending,
        input.screenOf?.(instance.id),
        input.summaryOf?.(instance.id),
      );
      // c-homeend: an ended row speaks the shared endReason sentence — the
      // human label and the red/neutral decision live in one module for every
      // session surface. The raw lastError never owns the body: it survives
      // only as `bodyDetail` for the tooltip. The sentence is built from the
      // label ITSELF, not nextStep()'s text: that projection puts a durable
      // pending interaction ahead of its end-reason branch, so a restarted
      // session whose approval/question outlived the process would otherwise
      // keep showing the stale question. Live rows keep the UO-3
      // error-as-body rule (a journaled API error on a running process).
      const canResume =
        status === "exited" &&
        instance.capabilities.capabilities.resume?.state === "supported";
      const homeRank = rank(instance, input.childLiveOf?.(instance.id) === true);
      const end = endReason(instance);
      const ended = end !== null && status !== "unknown" ? end : null;
      // D-057 OA6 (ma-sdk-state r2 item 7): a live row's retryable turn error
      // comes from the durable lastTurnError marker (cleared by the next turn
      // start). It is shown verbatim but is NOT a red process failure; a
      // genuine process error (homeError) keeps the error treatment.
      //
      // c-question-alert r3 item 8: UNKNOWN connectivity/lifecycle takes
      // precedence over BOTH error channels — a disconnect (or a reconcile
      // after reconnect) on an instance that still carries lastTurnError must
      // read 状态待确认, never present the stale turn text as the live state.
      const statusUnknown = status === "unknown";
      const turnError = ended || statusUnknown ? null : homeTurnError(instance);
      const processError = ended || statusUnknown || turnError
        ? null
        : homeError(instance, input.eventsOf?.(instance.id));
      const body = ended
        ? { text: canResume ? `${ended.label} · 可恢复` : ended.label, isError: false }
        : turnError
          ? { text: turnError, isError: false }
          : homeBody(status, step, processError);
      rows.set(instance.id, {
        id: instance.id,
        instance,
        title: input.titleOf(instance.id) || "会话",
        status,
        rank: homeRank,
        kind: instance.kind,
        stateWord: rankStateWord(homeRank),
        body: body.text,
        bodyIsError: body.isError,
        bodyTone: ended?.tone ?? null,
        bodyDetail: ended?.detail ?? null,
        contextPct: input.rollupOf(instance.id)?.contextPct ?? null,
        contextPctApproximate: input.rollupOf(instance.id)?.contextPctApproximate ?? false,
        blocked: status === "blocked",
        canResume,
        timeLabel: formatListTime(instance.updatedAt, nowMs),
        updatedAt: instance.updatedAt,
      });
    }
  }
  return rows;
}

function rowTime(row: Pick<RankRow, "updatedAt">): number {
  return Date.parse(row.updatedAt) || 0;
}

/**
 * The fields the shared comparator reads. `HomeRow` satisfies it, and so does
 * the small row the desktop list builds from an instance — that list has no
 * context ring or body sentence of its own.
 */
export type RankRow = Pick<HomeRow, "id" | "rank" | "title" | "updatedAt">;

/**
 * The one comparator both surfaces use. Rank first (blocked above done-unseen
 * above working above awaiting-wake above starting above idle above exited),
 * newest change first inside a rank. `list` ordering is the device preference
 * that sorts by title instead of time, still inside the rank.
 */
export function compareRanked(a: RankRow, b: RankRow, order: HomeOrder = "clock"): number {
  const byRank = rankOrder(a.rank) - rankOrder(b.rank);
  if (byRank !== 0) return byRank;
  if (order === "list") return a.title.localeCompare(b.title) || a.id.localeCompare(b.id);
  return rowTime(b) - rowTime(a) || a.id.localeCompare(b.id);
}

function compareGroups(a: HomeGroup, b: HomeGroup, order: HomeOrder): number {
  if (a.id === OTHER_SPACE) return 1;
  if (b.id === OTHER_SPACE) return -1;
  if (order === "list") {
    return a.project.localeCompare(b.project) || a.id.localeCompare(b.id);
  }
  const latest = (group: HomeGroup) =>
    group.rows.reduce((max, row) => Math.max(max, rowTime(row)), 0);
  return latest(b) - latest(a) || a.id.localeCompare(b.id);
}

function groupDots(rows: readonly HomeRow[]): HomeGroup["dots"] {
  const dots = { blocked: 0, unseen: 0, working: 0, wake: 0, starting: 0, idle: 0, exited: 0 };
  for (const row of rows) {
    if (row.rank === "blocked") dots.blocked += 1;
    else if (row.rank === "done-unseen") dots.unseen += 1;
    else if (row.rank === "working") dots.working += 1;
    else if (row.rank === "awaiting-wake") dots.wake += 1;
    else if (row.rank === "starting") dots.starting += 1;
    else if (row.rank === "exited") dots.exited += 1;
    else dots.idle += 1;
  }
  return dots;
}

/**
 * Query filtering, per-group row shaping and ordering over pre-derived rows.
 * Cheap to run on every keystroke — `nextStep()`/error derivation is not in
 * here (see `buildHomeRows`).
 *
 * Rows come back in rank order (newest first within a rank). The caller folds
 * idle/exited rows and whole quiet projects; this function only marks them.
 * A non-empty query is a search, and a search returns every match unfolded.
 */
export function arrangeHomeGroups(input: {
  spaces: Space[];
  rows: ReadonlyMap<string, HomeRow>;
  needle: string;
  order: HomeOrder;
  hostNameOf: HomeRowsInput["hostNameOf"];
  branchOf?: HomeRowsInput["branchOf"];
  titleOf: HomeRowsInput["titleOf"];
}): HomeGroup[] {
  const needle = input.needle.trim().toLowerCase();
  // The QuickFind ranker is the fixed title > project(Space) > host > id
  // boundary. Mobile search drops id hits: the phone home search promises
  // 标题 / 项目 / 主机 only, and an opaque id must not leak out here.
  const ranked = rankQuickFind({
    spaces: input.spaces,
    query: needle,
    titleOf: input.titleOf,
    hostNameOf: input.hostNameOf,
    connection: "live",
  });
  const allowed = new Set(
    (needle ? ranked.hits.filter((hit) => hit.field !== "id") : ranked.hits).map(
      (hit) => hit.instance.id,
    ),
  );

  const groups: HomeGroup[] = [];
  for (const space of input.spaces) {
    const rows: HomeRow[] = [];
    for (const instance of space.instances) {
      if (needle && !allowed.has(instance.id)) continue;
      const row = input.rows.get(instance.id);
      if (row) rows.push(row);
    }
    if (!rows.length) continue;
    rows.sort((a, b) => compareRanked(a, b, input.order));
    const quiet = rows.every((row) => !rankIsActive(row.rank));
    groups.push({
      id: space.id,
      project: space.name,
      hostName: input.hostNameOf(space.hostId),
      branch: input.branchOf?.(space.id) ?? null,
      blockedCount: space.blockedCount,
      dots: groupDots(rows),
      quiet,
      rows,
    });
  }
  groups.sort((a, b) => compareGroups(a, b, input.order));
  return groups;
}

export function buildHomeGroups(input: HomeRowsInput): HomeGroup[] {
  const rows = buildHomeRows(input);
  return arrangeHomeGroups({
    spaces: input.spaces,
    rows,
    needle: input.query,
    order: input.order,
    hostNameOf: input.hostNameOf,
    branchOf: input.branchOf,
    titleOf: input.titleOf,
  });
}

/**
 * Stable signature over everything that can change a derived row's pixels
 * (plus the pending-instance set) — and the end-reason tooltip detail, which
 * is not a pixel but must refresh when a terminal row's last_error changes
 * behind an unchanged label (c-homeend r2). When two Hub snapshots sign the
 * same, the phone list keeps the previous `buildHomeRows()` result and React
 * bails out — the inbox badge changing alone (the same one pending
 * interaction, more interactions arriving for an already-blocked session)
 * never commits it.
 */
export function homeRowsSignature(
  rows: ReadonlyMap<string, HomeRow>,
  pendingInstanceIds: readonly string[],
  spaces: readonly Space[],
  hostNameOf: HomeRowsInput["hostNameOf"],
): string {
  const rowPart = [...rows.values()]
    .map(
      (row) =>
        `${row.id}|${row.status}|${row.rank}|${row.kind}|${row.stateWord ?? "-"}|${row.title}|${row.body}|${row.bodyIsError ? 1 : 0}|${row.bodyTone ?? "-"}|${row.bodyDetail ?? "-"}|${row.contextPct ?? "-"}|${row.contextPctApproximate ? "a" : "x"}|${row.blocked ? 1 : 0}|${row.canResume ? 1 : 0}|${row.timeLabel}`,
    )
    .join("\n");
  // Prefs/closed tabs and workspace membership change WHICH instances a space
  // presents (and the group's blocked count); spaces are the cached selector's
  // buildSpaces() output.
  const spacePart = spaces
    .map(
      (space) =>
        `${space.id}:${space.name}:${space.hostId ?? "-"}:${space.blockedCount}:${space.instances
          .map((instance) => instance.id)
          .join(",")}`,
    )
    .join("\n");
  // Host renames change the group header's title tooltip.
  const hostPart = spaces
    .map((space) => (space.hostId ? `${space.hostId}=${hostNameOf(space.hostId)}` : ""))
    .filter(Boolean)
    .join(",");
  return `${[...new Set(pendingInstanceIds)].sort().join(",")}⟦${spacePart}⟧${hostPart}⟦${rowPart}`;
}

/**
 * D-050 (task-model task 5): the task grouping layer the `/m` home stacks
 * above its existing project + git branch session groups. This is only an
 * additive projection — it delegates wholesale to the task list's pure
 * `buildTaskGroups()` (需要你 first, project+branch groups with the
 * buildSpaces() blocked count, parent/child nesting, SE-nn keys, 已归档
 * folded away) and renders no second transcript: every task row links to the
 * shared `/s/:id`.
 */
export type HomeTaskLayerInput = {
  tasks: readonly Task[];
  instances: readonly Instance[];
  interactions: readonly Pick<Interaction, "instanceId" | "state">[];
  spaces: readonly Space[];
  projectName?: (projectId: string) => string | null | undefined;
  branchOfSpace?: (spaceId: string) => string | null | undefined;
  query?: string;
};

export function buildHomeTaskLayer(input: HomeTaskLayerInput): TaskListGroup[] {
  return buildTaskGroups({
    tasks: input.tasks,
    instances: input.instances,
    interactions: input.interactions,
    spaces: input.spaces,
    projectName: input.projectName,
    branchOfSpace: input.branchOfSpace,
    query: input.query,
  });
}
