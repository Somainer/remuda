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
import type { PushStatus } from "../../lib/push";

/**
 * Pure derivation for the phone inbox (`/m/inbox`, D-049 / ui-spec §2.5,
 * §4.7). Two tiers and nothing else:
 *
 *  - 待你处理: every non-settled interaction projected to
 *    pending / answering / paused (the same projection ApprovalsPage uses,
 *    so mobile and desktop never disagree about queue membership);
 *  - 进行中 · 最近: working / idle / exited instances, newest activity first,
 *    minus instances already represented by a tier-1 row (a blocked session
 *    is never also advertised as working).
 *
 * Expired / superseded interactions are not a third tier: the phone inbox
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
 * status sentence. Then the live phrase projected from the journal tail
 * (`liveSummary`, itself verbatim message/workflow text), then the
 * interaction request's own text. `null` means "nothing to print" and the
 * row renders no subtitle line.
 */
export function latestEventText(
  instance: Instance | undefined,
  phrase: string | undefined,
  interaction?: Interaction,
): string | null {
  const error = instance?.lastError?.trim();
  if (error) return error;
  const live = phrase?.trim();
  if (live) return live;
  if (interaction) {
    const text = interactionRequestText(interaction).trim();
    if (text) return text;
  }
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
  updatedAt: string;
};

export type InboxRows = {
  /** 待你处理 */
  pending: InboxInteractionRow[];
  /** 进行中 · 最近 */
  recent: InboxInstanceRow[];
};

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

const RECENT_STATUSES: ReadonlySet<UiStatus> = new Set(["working", "idle", "exited"]);

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
  for (const item of source.interactions) {
    if (kind !== "all" && item.kind !== kind) continue;
    const instance = instanceById.get(item.instanceId);
    const host = hostById.get(item.hostId);
    const uiState = projectInteraction(item, {
      answering: Boolean(source.answering[item.id]),
      host,
      connectivity: instance?.connectivity,
      deviceId: source.deviceId,
    });
    if (uiState !== "pending" && uiState !== "answering" && uiState !== "paused") continue;

    const options =
      item.request.kind === "approval" || item.request.kind === "plan-review"
        ? item.request.options
        : [];
    pending.push({
      rowType: "interaction",
      rowId: item.id,
      interactionId: item.id,
      instanceId: item.instanceId,
      hostId: item.hostId,
      interactionKind: item.kind,
      carrier: item.carrier,
      headline: interactionHeadline(item),
      subtitle: latestEventText(instance, source.phrases[item.instanceId], item),
      hostLabel: source.hostName(item.hostId),
      workspaceLabel: source.workspaceLabel(instance?.workspaceId ?? ""),
      harness: instance?.kind ?? "—",
      timeLabel: formatListTime(item.updatedAt || item.createdAt, nowMs),
      contextPct: contextPctOf(instance, source.rollups),
      uiState,
      answerable: item.answerable,
      options,
      goAnswer: item.kind === "question",
      focused: focus === item.id,
      createdAt: item.createdAt,
    });
  }
  pending.sort(byRecency);

  // A blocked instance already has a row in the interaction tier (kind
  // filtering can hide it; it is still blocked — never advertise it as
  // working or idle).
  const blockedInstanceIds = new Set(
    source.interactions
      .filter((item) => {
        const instance = instanceById.get(item.instanceId);
        const host = hostById.get(item.hostId);
        const state = projectInteraction(item, {
          answering: Boolean(source.answering[item.id]),
          host,
          connectivity: instance?.connectivity,
          deviceId: source.deviceId,
        });
        return state === "pending" || state === "answering" || state === "paused";
      })
      .map((item) => item.instanceId),
  );

  const recent: InboxInstanceRow[] = [];
  for (const instance of source.instances) {
    if (blockedInstanceIds.has(instance.id)) continue;
    const status = projectStatus(instance);
    if (!RECENT_STATUSES.has(status)) continue;
    recent.push({
      rowType: "instance",
      rowId: instance.id,
      instanceId: instance.id,
      hostId: instance.hostId,
      title: source.titleOf(instance.id) || "会话",
      subtitle: latestEventText(instance, source.phrases[instance.id]),
      hostLabel: source.hostName(instance.hostId),
      workspaceLabel: source.workspaceLabel(instance.workspaceId),
      harness: instance.kind,
      timeLabel: formatListTime(instance.updatedAt, nowMs),
      contextPct: contextPctOf(instance, source.rollups),
      status,
      updatedAt: instance.updatedAt,
    });
  }
  recent.sort((a, b) => byRecency({ createdAt: a.updatedAt, rowId: a.rowId }, { createdAt: b.updatedAt, rowId: b.rowId }));

  return { pending, recent };
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
