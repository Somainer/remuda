import type { Host, Instance, UiStatus } from "../../types/instance";
import type { DecisionOption, Interaction } from "../../types/interaction";
import type { Id } from "../../types/wire";
import type { UsageRollup } from "../session/contextUsage";
import { formatListTime } from "../../lib/format";
import {
  projectInteraction,
  type InteractionUiState,
} from "../../lib/interactionStatus";
import { projectStatus } from "../../lib/status";
import { endReason, type EndReason } from "../../lib/endReason";
import type { PushStatus } from "../../lib/push";

/**
 * Pure derivation for the phone inbox (`/m/inbox`, D-049 / ui-spec §2.5,
 * §4.7). Two tiers plus one quiet end group:
 *
 *  - 待你处理: every non-settled interaction projected to
 *    pending / answering / paused (the same projection ApprovalsPage uses,
 *    so mobile and desktop never disagree about queue membership);
 *  - 进行中 · 最近: working / idle instances, newest activity first,
 *    minus instances already represented by a tier-1 row (a blocked session
 *    is never also advertised as working). A session that is no longer
 *    running can never sit under a heading that says 进行中;
 *  - 最近结束: exited/failed instances, newest first, capped at
 *    {@link MAX_ENDED_ROWS}. The shell renders it collapsed; rows carry the
 *    human {@link EndReason} sentence (red only for a proven failure), never
 *    the raw wire code.
 *
 * Expired / superseded interactions are not a fourth tier: the phone inbox
 * deliberately has no 已离队 section (§11.3 observed two tiers, not three).
 */

export const INBOX_KINDS = [
  "all",
  "approval",
  "question",
  "plan-review",
  "elicitation",
] as const;
export type InboxKind = (typeof INBOX_KINDS)[number];

const KIND_SET = new Set<string>(INBOX_KINDS);

export function parseKindParam(value: string | null): InboxKind {
  return value && KIND_SET.has(value) ? (value as InboxKind) : "all";
}

export const KIND_SEGMENT_LABEL: Record<InboxKind, string> = {
  all: "全部",
  approval: "审批",
  question: "提问",
  "plan-review": "计划",
  elicitation: "表单",
};

/** Kind/tool line over the request body. Mirrors ApprovalsPage's kindLabel. */
export function interactionHeadline(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.title;
  if (item.request.kind === "question") {
    return item.carrier === "native-tty" ? "终端提问" : "AskUserQuestion";
  }
  if (item.request.kind === "plan-review") return "计划";
  return item.request.title;
}

/**
 * The request's own text, verbatim. Mirrors ApprovalsPage's preview(): the
 * approval description, the terminal question lines, the composed
 * AskUserQuestion line, or the plan / form title. This is the fallback
 * subtitle when the instance has neither an error nor a journal phrase.
 */
export function interactionRequestText(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.description;
  if (item.request.kind === "question") {
    return item.carrier === "native-tty"
      ? item.request.fields.map((field) => field.description ?? field.title).join("\n")
      : `问你 ${item.request.fields.length} 题 · AskUserQuestion`;
  }
  return item.request.title;
}

/**
 * Subtitle text for a row: the latest thing a human can read, verbatim.
 *
 * Error-first by construction: `instance.lastError` wins whenever present and
 * is printed exactly as reported — never softened, translated or wrapped in a
 * status sentence. For an interaction row the interaction's own request text
 * comes next: on a blocked turn that request IS the latest actionable event,
 * and showing it verbatim is what makes the one-tap decision possible (the
 * §B.3.3 row is `Bash rm -rf …`), so an older journal phrase must never
 * bury it. Only instance rows (进行中 · 最近) fall through to the live phrase
 * projected from the journal tail. `null` means "nothing to print" and the
 * row renders no subtitle line.
 */
export function latestEventText(
  instance: Instance | undefined,
  phrase: string | undefined,
  interaction?: Interaction,
): string | null {
  const error = instance?.lastError?.trim();
  if (error) return error;
  if (interaction) {
    const text = interactionRequestText(interaction).trim();
    if (text) return text;
  }
  const live = phrase?.trim();
  if (live) return live;
  return null;
}

function contextPctOf(
  instance: Instance | undefined,
  rollups: Record<string, UsageRollup>,
): number | null {
  const id = instance?.id;
  const pct =
    (id ? rollups[id]?.contextPct : null) ?? instance?.usageRollup?.contextPct ?? null;
  return typeof pct === "number" && Number.isFinite(pct) ? pct : null;
}

/**
 * Screen-reader (and tooltip) readout for the context remaining ring. A null
 * pct renders no ring, so it returns null; otherwise the value is clamped to
 * 0..100 the same way the SVG is.
 */
export function contextRingLabel(pct: number | null): string | null {
  if (pct == null) return null;
  return `上下文剩余 ${Math.max(0, Math.min(100, pct))}%`;
}

export type InboxInteractionRow = {
  rowType: "interaction";
  /** Focus target: equals the interaction id (?focus=). */
  rowId: Id;
  interactionId: Id;
  instanceId: Id;
  hostId: Id;
  interactionKind: Interaction["kind"];
  carrier: Interaction["carrier"];
  headline: string;
  /** Latest event text verbatim; null renders no subtitle line. */
  subtitle: string | null;
  hostLabel: string;
  workspaceLabel: string;
  harness: string;
  timeLabel: string;
  /** 0..100; null renders neither ring nor number. */
  contextPct: number | null;
  uiState: Extract<InteractionUiState, "pending" | "answering" | "paused">;
  answerable: boolean;
  /** One-tap options for approval / plan-review rows. */
  options: DecisionOption[];
  /** AskUserQuestion / native-tty rows are answered in the session page. */
  goAnswer: boolean;
  focused: boolean;
  createdAt: string;
  /**
   * Deep-equality signature of every field the card renders. The 2 s
   * interaction poll and the 2.5 s summary tick re-parse state into fresh
   * objects; an equal sig lets the memoized card skip re-rendering
   * (c-inboxperf, evidence inbox-perf-1.md).
   */
  sig: string;
};

export type InboxInstanceRow = {
  rowType: "instance";
  rowId: Id;
  instanceId: Id;
  hostId: Id;
  title: string;
  subtitle: string | null;
  hostLabel: string;
  workspaceLabel: string;
  harness: string;
  timeLabel: string;
  contextPct: number | null;
  status: UiStatus;
  /**
   * Human end reason for 最近结束 rows; null on a live 进行中 row. When set,
   * `subtitle` is `end.label` and the raw machine code lives only in
   * `end.detail`; the card paints red iff `end.tone === "failed"`.
   */
  end: EndReason | null;
  updatedAt: string;
  /** Deep-equality signature for the memoized recent-row card. */
  sig: string;
};

export type InboxRows = {
  /** 待你处理 */
  pending: InboxInteractionRow[];
  /** 进行中 · 最近 (live working/idle rows only). */
  recent: InboxInstanceRow[];
  /** 最近结束 (ended rows, newest first, capped at MAX_ENDED_ROWS). */
  ended: InboxInstanceRow[];
};

/** At most this many ended rows are offered behind the collapsed 最近结束 group. */
export const MAX_ENDED_ROWS = 10;

export type InboxSource = {
  interactions: Interaction[];
  instances: Instance[];
  hosts: Host[];
  /** hub.answering: interaction ids POSTed locally, awaiting journal receipt. */
  answering: Record<string, true>;
  /** hub.summaries: live phrases projected from journal tails. */
  phrases: Record<string, string>;
  /** hub.usageRollup: Hub-computed context rollups. */
  rollups: Record<string, UsageRollup>;
  deviceId: string;
  titleOf: (instanceId: Id) => string;
  hostName: (hostId: Id) => string;
  workspaceLabel: (workspaceId: Id) => string;
  nowMs?: number;
};

const RECENT_STATUSES: ReadonlySet<UiStatus> = new Set(["working", "idle"]);
const ACTIVE_INTERACTION_STATES: ReadonlySet<InteractionUiState> = new Set([
  "pending",
  "answering",
  "paused",
]);

function isActiveUiState(
  state: InteractionUiState,
): state is Extract<InteractionUiState, "pending" | "answering" | "paused"> {
  return ACTIVE_INTERACTION_STATES.has(state);
}

function byRecency(a: { createdAt: string; rowId: string }, b: { createdAt: string; rowId: string }): number {
  const ta = Date.parse(a.createdAt);
  const tb = Date.parse(b.createdAt);
  if (ta !== tb) return tb - ta;
  return a.rowId < b.rowId ? 1 : a.rowId > b.rowId ? -1 : 0;
}

/**
 * Project the two tiers. Pure: the component passes store state plus its
 * accessors, and `opts.kind` / `opts.focus` mirror the /approvals URL query.
 * The kind segment filters the interaction tier exactly as /approvals does;
 * the recent tier is instance projection and stays untouched by it.
 *
 * Each interaction is projected exactly once (the blocked-instance set is
 * built in the same pass; it deliberately ignores the kind filter, since a
 * blocked instance stays blocked even while its row is filtered out).
 */
export function deriveInboxRows(
  source: InboxSource,
  opts: { kind?: InboxKind; focus?: string | null } = {},
): InboxRows {
  const kind = opts.kind ?? "all";
  const focus = opts.focus ?? null;
  const nowMs = source.nowMs ?? Date.now();
  const instanceById = new Map(source.instances.map((instance) => [instance.id, instance]));
  const hostById = new Map(source.hosts.map((host) => [host.id, host]));

  const pending: InboxInteractionRow[] = [];
  // A blocked instance already has a row in the interaction tier (kind
  // filtering can hide it; it is still blocked — never advertise it as
  // working or idle).
  const blockedInstanceIds = new Set<Id>();

  for (const item of source.interactions) {
    const instance = instanceById.get(item.instanceId);
    const host = hostById.get(item.hostId);
    const uiState = projectInteraction(item, {
      answering: Boolean(source.answering[item.id]),
      host,
      connectivity: instance?.connectivity,
      deviceId: source.deviceId,
    });
    if (!isActiveUiState(uiState)) continue;
    // A blocked instance already has a row in the interaction tier (kind
    // filtering can hide the row; the instance is still blocked — never
    // advertise it as working or idle).
    blockedInstanceIds.add(item.instanceId);
    if (kind !== "all" && item.kind !== kind) continue;

    const options =
      item.request.kind === "approval" || item.request.kind === "plan-review"
        ? item.request.options
        : [];
    const hostLabel = source.hostName(item.hostId);
    const workspaceLabel = source.workspaceLabel(instance?.workspaceId ?? "");
    const timeLabel = formatListTime(item.updatedAt || item.createdAt, nowMs);
    const subtitle = latestEventText(instance, source.phrases[item.instanceId], item);
    const contextPct = contextPctOf(instance, source.rollups);
    const focused = focus === item.id;
    pending.push({
      rowType: "interaction",
      rowId: item.id,
      interactionId: item.id,
      instanceId: item.instanceId,
      hostId: item.hostId,
      interactionKind: item.kind,
      carrier: item.carrier,
      headline: interactionHeadline(item),
      subtitle,
      hostLabel,
      workspaceLabel,
      harness: instance?.kind ?? "—",
      timeLabel,
      contextPct,
      uiState,
      answerable: item.answerable,
      options,
      goAnswer: item.kind === "question",
      focused,
      createdAt: item.createdAt,
      sig: JSON.stringify({
        t: "i",
        // Every interaction field the card/actions read; `request` is a
        // fresh parse on every poll so compare by content, not identity.
        i: [
          item.state,
          item.kind,
          item.carrier,
          item.answerable,
          item.createdAt,
          item.updatedAt,
          item.deadline,
          item.request,
        ],
        n: instance
          ? [
              instance.connectivity,
              instance.lifecycle,
              instance.activity,
              instance.kind,
              instance.lastError,
              instance.usageRollup,
            ]
          : null,
        // contextPct also reads source.rollups (a separate store slice).
        v: [hostLabel, workspaceLabel, subtitle, timeLabel, contextPct, focused ? 1 : 0, uiState],
      }),
    });
  }
  pending.sort(byRecency);

  const recent: InboxInstanceRow[] = [];
  const ended: InboxInstanceRow[] = [];
  for (const instance of source.instances) {
    if (blockedInstanceIds.has(instance.id)) continue;
    const status = projectStatus(instance);
    const isEnded = status === "exited";
    // An ended/failed row can never read as 进行中: live working/idle rows
    // fill the recent tier, terminal rows the quiet 最近结束 group.
    if (!isEnded && !RECENT_STATUSES.has(status)) continue;
    const hostLabel = source.hostName(instance.hostId);
    const workspaceLabel = source.workspaceLabel(instance.workspaceId);
    const title = source.titleOf(instance.id) || "会话";
    // Ended rows show the shared human sentence; the raw code survives only in
    // end.detail (the card tooltip). Live rows keep the error/phrase order.
    const end = isEnded ? endReason(instance) : null;
    const subtitle = end
      ? end.label
      : latestEventText(instance, source.phrases[instance.id]);
    const timeLabel = formatListTime(instance.updatedAt, nowMs);
    const contextPct = contextPctOf(instance, source.rollups);
    const row: InboxInstanceRow = {
      rowType: "instance",
      rowId: instance.id,
      instanceId: instance.id,
      hostId: instance.hostId,
      title,
      subtitle,
      hostLabel,
      workspaceLabel,
      harness: instance.kind,
      timeLabel,
      contextPct,
      status,
      end,
      updatedAt: instance.updatedAt,
      sig: JSON.stringify({
        t: "n",
        n: [
          instance.connectivity,
          instance.lifecycle,
          instance.activity,
          instance.kind,
          instance.lastError,
          instance.usageRollup,
          instance.updatedAt,
          instance.exit,
        ],
        // contextPct also reads source.rollups (a separate store slice).
        v: [
          hostLabel,
          workspaceLabel,
          title,
          subtitle,
          end?.label,
          end?.detail,
          end?.tone,
          timeLabel,
          contextPct,
          status,
        ],
      }),
    };
    (isEnded ? ended : recent).push(row);
  }
  recent.sort((a, b) => byRecency({ createdAt: a.updatedAt, rowId: a.rowId }, { createdAt: b.updatedAt, rowId: b.rowId }));
  ended.sort((a, b) => byRecency({ createdAt: a.updatedAt, rowId: a.rowId }, { createdAt: b.updatedAt, rowId: b.rowId }));
  // The collapsed group is a quiet glance: cap the rows, newest first.
  if (ended.length > MAX_ENDED_ROWS) ended.length = MAX_ENDED_ROWS;

  return { pending, recent, ended };
}

/**
 * Permission banner derivation (D-049 §6). The banner is the single
 * phone entry point for push, shown only while notifications are not
 * granted; it never requests permission itself — the component wires the
 * 开启 tap to subscribePush(), and the iOS-not-standalone variant only
 * offers home-screen guidance.
 */
export type PushBannerState =
  | { show: false }
  | { show: true; mode: "enable" }
  | { show: true; mode: "homescreen" };

export function derivePushBanner(status: PushStatus | null): PushBannerState {
  if (!status) return { show: false };
  if (status.permission === "granted") return { show: false };
  // iOS Safari has no Notification API until the PWA is added to the home
  // screen; the only honest action is the home-screen hint.
  if (status.needsHomeScreen) return { show: true, mode: "homescreen" };
  // No Notification API at all (and not the iOS-homescreen case): nothing a
  // tap could enable, so no banner.
  if (status.permission === "unsupported") return { show: false };
  // default: offer 开启. denied: still shown (the query only says "not
  // granted"); a tap re-attempts and the user can dismiss.
  return { show: true, mode: "enable" };
}
