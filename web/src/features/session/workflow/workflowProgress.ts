/**
 * Pure projection for the Workflow timeline card (workbench batch W).
 *
 * The Node owns what is *true*; this module only shapes observations into the
 * view model the card draws. Nothing here touches React or the DOM, so the
 * folding / grid / wording rules are unit-testable in isolation.
 *
 * Product decisions (approved mock):
 * - card status: running/completed/failed/killed/paused
 * - agent state: queued/running/done/failed/killed
 * - a phase with more than {@link GRID_THRESHOLD} agents lays out in columns;
 * - after that, a phase taller than {@link FOLD_THRESHOLD} agent rows folds its
 *   trailing *quiet* (done/queued) rows behind 「… 还有 n 个」; running and
 *   failed rows are never folded;
 * - ≤560px hides model short name + latest tool (the {@link SOFT_META} set);
 * - when the harness cannot give phase detail the card degrades to a plain tool
 *   row plus one note ({@link degradedCard}).
 */
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../../types/generated";
import { knowledgeValue } from "../../../types/command";

/** Card-level status. `paused` is protocol-reserved; no producer emits it yet. */
export type WfStatus = "running" | "completed" | "failed" | "killed" | "paused";
/** Per-agent state. */
export type WfState = "queued" | "running" | "done" | "failed" | "killed";

/** A phase with more agents than this switches to column layout. */
export const GRID_THRESHOLD = 8;
/** A phase shows at most this many agent rows; the quiet tail folds. */
export const FOLD_THRESHOLD = 12;

/** Meta hidden under the 560px breakpoint so every agent stays one line. */
export const SOFT_META = ["model", "latestTool"] as const;

export interface WfAgent {
  id: string;
  label: string;
  state: WfState;
  model?: string;
  latestTool?: string;
  durationMs?: number;
  tokens?: number;
  calls?: number;
  /** Attempt marker; rendered only when > 1. */
  attempt?: number;
  /** Agent spawn timestamp (epoch ms). */
  startedAtMs?: number;
  /** Agent stop timestamp (epoch ms); absent while running. */
  endedAtMs?: number;
  /** Newest agent-transcript progress timestamp (epoch ms). */
  lastProgressAtMs?: number;
}

/** The three per-agent clocks, defined once for the card and its tests. */
export interface WfClocks {
  /** endedAt − startedAt; while running, now − startedAt. */
  durationMs?: number;
  /** endedAt (or now while running) − lastProgressAt. */
  idleMs?: number;
  /** startedAt − the run's launchedAt. */
  queueMs?: number;
}

export interface WfTotals {
  totalKnown: boolean;
  agentsTotal: number;
  done: number;
  failed: number;
  killed: number;
  running: number;
  queued: number;
  tokens: number;
  calls: number;
  elapsedMs: number;
}

export interface WfPhaseView {
  id: string;
  title: string;
  /** All agents in index order. */
  agents: WfAgent[];
  /** Index-ordered agents; running/failed always retained here. */
  pinned: WfAgent[];
  /** Quiet (done/queued) agents kept before the fold, in index order. */
  head: WfAgent[];
  /** Quiet agents behind the 「还有 n 个」 toggle, in index order. */
  folded: WfAgent[];
  /** True when this phase uses column layout. */
  grid: boolean;
  /** True when a fold toggle is rendered. */
  canFold: boolean;
  total: number;
  done: number;
  running: number;
  failed: number;
  killed: number;
  queued: number;
  /** Span of the phase, last agent end − first agent start. */
  durationMs?: number;
  /** Summed tokens across the phase's agents; undefined when none reported. */
  tokens?: number;
  /** Summed tool calls across the phase's agents; undefined when none reported. */
  calls?: number;
  /** Header count, e.g. `2/4` or `5/5 完成`. */
  countText: string;
  /** Right-aligned meta, e.g. `1 运行中 · 3m 42s` / `全部排队中`. */
  metaText: string;
  /** Expanded on first paint while the run is alive. */
  expandedByDefault: boolean;
}

export interface WfCard {
  /** False → the caller renders a plain tool row plus `note`. */
  detailed: boolean;
  name: string;
  description?: string;
  status: WfStatus;
  phases: WfPhaseView[];
  totals: WfTotals;
  /** Run launch instant (epoch ms); the per-agent queue-wait origin. */
  launchedAtMs?: number;
  /** 0–100 fill for the dotted progress rail. */
  railPct: number;
  /** 「当前 <phase>: <agent>」 while running. */
  live?: { phase: string; agent: string };
  /** Terminal one-line result. */
  summary?: string;
  /** Why detail is unavailable, when `detailed` is false. */
  note?: string;
}

function isQuiet(state: WfState): boolean {
  return state === "done" || state === "queued";
}

export function num(value?: number | string | null): number | undefined {
  if (typeof value === "number") return Number.isFinite(value) ? value : undefined;
  if (typeof value === "string" && value.trim() !== "") {
    const n = Number(value);
    return Number.isFinite(n) ? n : undefined;
  }
  return undefined;
}

/** knowledgeValue for an optional/nullable protocol field. */
function kv(k?: { state: string; value?: string } | null): string | undefined {
  return k ? knowledgeValue(k) ?? undefined : undefined;
}

/** `3m 42s` / `0m 41s` / `—` for a missing duration. */
export function fmtDuration(ms?: number | null): string {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return "—";
  const total = Math.round(ms / 1000);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}m ${String(s).padStart(2, "0")}s`;
}

/** Parse an RFC3339 wire timestamp to epoch ms; undefined when absent/unparseable. */
export function tsMs(value?: string | null): number | undefined {
  if (typeof value !== "string" || value === "") return undefined;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : undefined;
}

const subMs = (a?: number, b?: number): number | undefined =>
  typeof a === "number" && typeof b === "number" && a >= b ? a - b : undefined;

/**
 * The three per-agent clocks (the single definition the card renders):
 *
 * - duration = endedAt − startedAt, or now − startedAt while running;
 * - idle = endedAt (or now while running) − lastProgressAt;
 * - queue wait = startedAt − the run's launchedAt.
 *
 * A missing input leaves the clock `undefined` (rendered as `—`), never 0.
 */
export function agentClocks(agent: WfAgent, launchedAtMs: number | undefined, nowMs: number): WfClocks {
  const running = agent.state === "running";
  const endAnchor = running ? nowMs : agent.endedAtMs;
  return {
    durationMs: subMs(endAnchor, agent.startedAtMs),
    idleMs: subMs(endAnchor, agent.lastProgressAtMs),
    queueMs: subMs(agent.startedAtMs, launchedAtMs),
  };
}

/**
 * Honest running header elapsed. Two clocks can bound it and neither alone is
 * always truthful:
 *
 * - `now − launchedAt` — live wall time only when the Node launched the run
 *   itself. On the discovery path launchedAt is the instant the journal first
 *   noticed an already-running run, so this UNDER-counts;
 * - the producer's `totals.elapsedMs` — derived from the earliest agent start,
 *   correct at emission but frozen between revisions, so it is extrapolated by
 *   the wall time since the snapshot arrived.
 *
 * Take the larger of the two while running; when launchedAt is absent (older
 * node streaming to the new web) the extrapolated snapshot alone keeps
 * ticking. Returns 0 when neither clock exists, which the header hides.
 */
export function headerElapsed(input: {
  running: boolean;
  snapshotMs: number;
  launchedAtMs?: number;
  nowMs: number;
  /** `now` at which snapshotMs was last observed; anchors extrapolation. */
  snapshotAnchorMs: number;
}): number {
  const { running, snapshotMs, launchedAtMs, nowMs, snapshotAnchorMs } = input;
  if (!running) return snapshotMs;
  const candidates: number[] = [];
  if (snapshotMs > 0) {
    candidates.push(snapshotMs + Math.max(0, nowMs - snapshotAnchorMs));
  }
  if (typeof launchedAtMs === "number") {
    candidates.push(Math.max(0, nowMs - launchedAtMs));
  }
  return candidates.length ? Math.max(...candidates) : 0;
}

/** Compact token count: `9.1k` / `312k` / `2.4M`. */
export function fmtTokens(tokens?: number | null): string {
  if (!tokens || tokens <= 0) return "—";
  if (tokens >= 1_000_000) {
    const v = tokens / 1_000_000;
    return `${trimNum(v)}M`;
  }
  if (tokens >= 1_000) {
    const v = tokens / 1_000;
    return `${trimNum(v)}k`;
  }
  return String(tokens);
}

function trimNum(v: number): string {
  // One decimal, but drop a trailing ".0" so 312000 → `312k`, not `312.0k`.
  const rounded = Math.round(v * 10) / 10;
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
}

function countState(agents: WfAgent[]): Pick<WfPhaseView, "done" | "running" | "failed" | "killed" | "queued"> {
  const c = { done: 0, running: 0, failed: 0, killed: 0, queued: 0 };
  for (const a of agents) c[a.state] += 1;
  return c;
}

/**
 * Split index-ordered agents into pinned (running/failed/killed, never folded),
 * a quiet head kept on screen, and the folded quiet tail.
 *
 * The fold row sits where the tail begins in index order; pinned agents keep
 * their original positions relative to the head. We keep the first
 * `FOLD_THRESHOLD - pinned.length` quiet agents (never negative) so the phase
 * never shows more than {@link FOLD_THRESHOLD} rows.
 */
export function foldAgents(agents: WfAgent[]): Pick<WfPhaseView, "pinned" | "head" | "folded"> {
  const pinned: WfAgent[] = [];
  const quiet: WfAgent[] = [];
  for (const agent of agents) {
    if (isQuiet(agent.state)) quiet.push(agent);
    else pinned.push(agent);
  }
  const budget = Math.max(0, FOLD_THRESHOLD - pinned.length);
  return {
    pinned,
    head: quiet.slice(0, budget),
    folded: quiet.slice(budget),
  };
}

/**
 * Re-interleave pinned and visible quiet agents back into index order, with the
 * fold marker inserted where the tail was removed. Returns rows plus the index
 * at which to render the 「还有 n 个」 toggle (or null).
 */
export function layoutRows(
  agents: WfAgent[],
): { rows: WfAgent[]; foldIndex: number | null; folded: WfAgent[] } {
  const { pinned, head, folded } = foldAgents(agents);
  if (folded.length === 0) {
    return { rows: agents, foldIndex: null, folded: [] };
  }
  const visible = new Set([...pinned, ...head].map((a) => a.id));
  const visibleSet = visible;
  // Marker position: just after the visible rows that precede the first folded
  // agent's index. A pinned (running/failed) row later in index order stays
  // visible and renders after the marker — those rows are never folded.
  const firstFoldedIndex = agents.findIndex((a) => !visibleSet.has(a.id));
  const rows: WfAgent[] = [];
  let foldIndex: number | null = null;
  agents.forEach((agent, index) => {
    if (!visibleSet.has(agent.id)) return;
    if (foldIndex === null && index > firstFoldedIndex) {
      foldIndex = rows.length;
    }
    rows.push(agent);
  });
  // All-quiet tail (no pinned rows after it): marker goes at the end.
  if (foldIndex === null) foldIndex = rows.length;
  return { rows, foldIndex, folded };
}

/**
 * Real phase span: the latest agent end minus the earliest agent start.
 *
 * A running agent's open end is taken at `nowMs` (the card's one-second hand
 * passes it while live). Returns undefined when no agent carries a start
 * timestamp, so the header renders the dash instead of inventing a zero.
 */
export function phaseDuration(agents: WfAgent[], nowMs?: number): number | undefined {
  let start: number | undefined;
  let end: number | undefined;
  for (const a of agents) {
    if (typeof a.startedAtMs !== "number") continue;
    start = Math.min(start ?? Infinity, a.startedAtMs);
    const anchor = a.endedAtMs ?? (a.state === "running" ? nowMs : undefined) ?? a.startedAtMs;
    end = Math.max(end ?? -Infinity, anchor);
  }
  if (start === undefined || end === undefined) return undefined;
  return Math.max(0, end - start);
}

/** Sum one agent metric; undefined when no agent reports it. */
function sumMetric(agents: WfAgent[], pick: (a: WfAgent) => number | undefined): number | undefined {
  let total: number | undefined;
  for (const a of agents) {
    const v = pick(a);
    if (typeof v === "number") total = (total ?? 0) + v;
  }
  return total;
}

function projectPhase(id: string, title: string, agents: WfAgent[], nowMs?: number): WfPhaseView {
  const counts = countState(agents);
  const grid = agents.length > GRID_THRESHOLD;
  const { folded } = foldAgents(agents);
  const canFold = folded.length > 0;
  const total = agents.length;
  const terminal = counts.done + counts.failed + counts.killed;
  const duration = phaseDuration(agents, nowMs);
  const tokens = sumMetric(agents, (a) => a.tokens);
  const calls = sumMetric(agents, (a) => a.calls);

  const tail: string[] = [];
  tail.push(fmtDuration(duration));
  if (tokens !== undefined) tail.push(`${fmtTokens(tokens)} tokens`);
  if (calls !== undefined) tail.push(`${calls} 次调用`);

  let metaText: string;
  if (total > 0 && terminal === total && counts.failed === 0 && counts.killed === 0) {
    metaText = tail.join(" · ");
  } else if (counts.running > 0) {
    metaText = [`${counts.running} 运行中`, ...tail].join(" · ");
  } else if (counts.failed > 0) {
    metaText = [`${counts.failed} 已失败`, ...tail].join(" · ");
  } else if (counts.killed > 0) {
    const head: string[] = [`${counts.killed} 已终止`];
    if (counts.queued) head.push(`${counts.queued} 排队中`);
    metaText = [...head, ...tail].join(" · ");
  } else if (counts.done === 0 && counts.queued === total) {
    metaText = "全部排队中";
  } else {
    metaText = tail.join(" · ");
  }

  const fullyDone = total > 0 && counts.done === total;
  const countText = fullyDone ? `${total}/${total} 完成` : `${terminal}/${total}`;

  return {
    id,
    title,
    agents,
    ...foldAgents(agents),
    grid,
    canFold,
    total,
    ...counts,
    durationMs: duration,
    tokens,
    calls,
    countText,
    metaText,
    expandedByDefault: counts.running > 0 || counts.failed > 0 || counts.killed > 0,
  };
}

function totalsFrom(agents: WfAgent[], elapsedMs: number, totalKnown: boolean, agentsTotal: number): WfTotals {
  const c = countState(agents);
  let tokens = 0;
  let calls = 0;
  for (const a of agents) {
    tokens += a.tokens ?? 0;
    calls += a.calls ?? 0;
  }
  return {
    totalKnown,
    agentsTotal,
    ...c,
    tokens,
    calls,
    elapsedMs,
  };
}

/** Map the protocol member-state word to a card agent state. */
export function memberState(state: string): WfState {
  switch (state) {
    case "completed":
      return "done";
    case "failed":
      return "failed";
    case "cancelled":
      return "killed";
    case "queued":
      return "queued";
    default:
      return "running";
  }
}

/** Map a run state to the card status; killed reads 「已终止」. */
export function runStatus(state: string): WfStatus {
  switch (state) {
    case "completed":
      return "completed";
    case "failed":
      return "failed";
    case "cancelled":
      return "killed";
    case "paused":
      return "paused";
    default:
      return "running";
  }
}

export interface ProjectInput {
  run: WorkflowRunPayload;
  phases: WorkflowPhasePayload[];
  members: WorkflowMemberPayload[];
  /** Phase titles in script order, keyed by phase id, when derived from meta. */
  phaseOrder?: { id: string; title: string }[];
  /** Current wall time (epoch ms); the running card passes its 1s hand. */
  nowMs?: number;
}

/** Build a fully-detailed card view model from protocol observations. */
export function projectWorkflow({ run, phases, members, phaseOrder, nowMs }: ProjectInput): WfCard {
  // Decision 6: an explicit producer note (old daemon, missing run dir) means
  // phase detail cannot exist — degrade rather than drawing an empty shell.
  // An observation stream with neither phases nor members is equally flat.
  if (run.note || (phases.length === 0 && members.length === 0)) {
    return degradedCard(
      kv(run.name) ?? kv(run.title) ?? "workflow",
      runStatus(run.state),
      run.note ?? "daemon 版本较旧，暂无阶段明细",
      Number(u64(run.totals?.elapsedMs) ?? 0),
    );
  }
  const status = runStatus(run.state);
  const name = kv(run.name) ?? kv(run.title) ?? "workflow";
  const description = kv(run.description);

  // Group members under their phase; members without a phase id land in a
  // synthetic bucket so they are never silently dropped.
  const byPhase = new Map<string, WfAgent[]>();
  const order: string[] = [];
  for (const phase of phases) {
    const id = phase.phaseId;
    if (!byPhase.has(id)) {
      byPhase.set(id, []);
      order.push(id);
    }
  }
  for (const m of members) {
    const id = m.phaseId ?? "unphased";
    if (!byPhase.has(id)) {
      byPhase.set(id, []);
      order.push(id);
    }
    byPhase.get(id)!.push({
      id: knowledgeValue(m.nativeAgentId) ?? m.memberId,
      label: knowledgeValue(m.label) ?? shortAgentId(knowledgeValue(m.nativeAgentId) ?? m.memberId),
      state: memberState(m.state),
      model: knowledgeValue(m.modelResolved) ?? knowledgeValue(m.modelRequested) ?? undefined,
      latestTool: kv(m.latestTool),
      durationMs: num(u64(m.durationMs)),
      tokens: num(u64(m.tokens)),
      calls: num(u64(m.calls)),
      attempt: num(u64(m.attempt)),
      startedAtMs: tsMs(m.startedAt),
      endedAtMs: tsMs(m.endedAt),
      lastProgressAtMs: tsMs(m.lastProgressAt),
    });
  }

  const titleOf = (id: string, index: number): string => {
    const phase = phases.find((p) => p.phaseId === id);
    const fromPayload = phase ? knowledgeValue(phase.label) : undefined;
    const fromOrder = phaseOrder?.find((p) => p.id === id)?.title;
    return fromPayload ?? fromOrder ?? `阶段 ${index + 1}`;
  };

  const phaseViews = order.map((id, index) => projectPhase(id, titleOf(id, index), byPhase.get(id) ?? [], nowMs));

  const allAgents: WfAgent[] = phaseViews.flatMap((p) => [...p.pinned, ...p.head, ...p.folded]);
  const totalsPayload = run.totals;
  const totalKnown = totalsPayload?.totalKnown ?? true;
  const agentsTotal =
    num(u64(totalsPayload?.agentsTotal)) ?? allAgents.length;
  const totals: WfTotals = totalsPayload
    ? {
        totalKnown,
        agentsTotal,
        done: Number(u64(totalsPayload.agentsDone) ?? 0),
        failed: Number(u64(totalsPayload.agentsFailed) ?? 0),
        killed: Number(u64(totalsPayload.agentsKilled) ?? 0),
        running: Number(u64(totalsPayload.agentsRunning) ?? 0),
        queued: Math.max(0, agentsTotal - (done0(totalsPayload) + run0(totalsPayload) + fail0(totalsPayload) + kill0(totalsPayload))),
        tokens: Number(u64(totalsPayload.tokens) ?? 0),
        calls: Number(u64(totalsPayload.calls) ?? 0),
        elapsedMs: Number(u64(totalsPayload.elapsedMs) ?? 0),
      }
    : totalsFrom(allAgents, 0, totalKnown, agentsTotal);

  const terminal = totals.done + totals.failed + totals.killed;
  const railPct = totalKnown && agentsTotal > 0 ? Math.min(100, Math.round((terminal / agentsTotal) * 100)) : 0;

  // Live line: the most recent running agent (members arrive in event order, so
  // the last running one is current), else the last failed/killed.
  let live: WfCard["live"];
  if (status === "running") {
    const runningPhase = [...phaseViews].reverse().find((p) => p.running > 0);
    const runner = runningPhase
      ? [...runningPhase.pinned].reverse().find((a) => a.state === "running")
      : undefined;
    if (runningPhase && runner) live = { phase: runningPhase.title, agent: runner.label };
  }

  return {
    detailed: true,
    name,
    description,
    status,
    phases: phaseViews,
    totals,
    launchedAtMs: tsMs(run.launchedAt),
    railPct,
    live,
    summary: kv(run.live?.summary),
    note: run.note ?? undefined,
  };
}

function done0(t: NonNullable<WorkflowRunPayload["totals"]>): number {
  return Number(u64(t.agentsDone) ?? 0);
}
function run0(t: NonNullable<WorkflowRunPayload["totals"]>): number {
  return Number(u64(t.agentsRunning) ?? 0);
}
function fail0(t: NonNullable<WorkflowRunPayload["totals"]>): number {
  return Number(u64(t.agentsFailed) ?? 0);
}
function kill0(t: NonNullable<WorkflowRunPayload["totals"]>): number {
  return Number(u64(t.agentsKilled) ?? 0);
}

/** A degraded card: plain row plus an explanation, never an empty shell. */
export function degradedCard(name: string, status: WfStatus, note: string, elapsedMs = 0): WfCard {
  return {
    detailed: false,
    name,
    status,
    phases: [],
    totals: {
      totalKnown: false,
      agentsTotal: 0,
      done: 0,
      failed: 0,
      killed: 0,
      running: status === "running" ? 1 : 0,
      queued: 0,
      tokens: 0,
      calls: 0,
      elapsedMs,
    },
    railPct: 0,
    note,
  };
}

function shortAgentId(id: string): string {
  // Keep the hex short and stable; ids are opaque strings.
  return id.length > 10 ? id.slice(0, 8) : id;
}

/** Read a protocol U64 scalar (`string` on the wire) defensively. */
function u64(value: unknown): string | number | undefined {
  if (value === null || value === undefined) return undefined;
  if (typeof value === "string") return value;
  if (typeof value === "number") return value;
  if (typeof value === "object" && value !== null) {
    const inner = (value as { value?: unknown }).value;
    if (typeof inner === "string" || typeof inner === "number") return inner;
  }
  return undefined;
}
