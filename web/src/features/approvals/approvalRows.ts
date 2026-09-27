import {
  projectInteraction,
  settledOnThisDevice,
  type InteractionUiState,
} from "../../lib/interactionStatus";
import { formatClock } from "../../lib/format";
import type { Host, Instance, UiStatus } from "../../types/instance";
import type { Interaction } from "../../types/interaction";

/**
 * Pure row derivation for the desktop approvals center
 * (`/approvals`). Extracted from the component so the memo depends on
 * concrete store slices and so the rules are unit-tested without rendering.
 *
 * Performance shape (c-inboxperf, evidence inbox-perf-1.md): the 2.7 s inbox
 * long task was React committing ~100 cards on every 2 s interaction poll; the
 * derivation itself stayed under 50 ms. This module therefore:
 *  - joins instances/hosts through O(1) Maps (was Array.find per row,
 *    O(interactions × instances));
 *  - projects every interaction exactly once;
 *  - drops interactions already settled on THIS device (they render nowhere,
 *    not even in 已离队) before doing the joins;
 *  - attaches a `sig` per row covering every store-derived field the card
 *    renders, so a memoized card bails out when a poll re-parses identical
 *    interactions (fresh object identity, equal content).
 */

export type ApprovalRow = {
  item: Interaction;
  instance: Instance | undefined;
  host: Host | undefined;
  uiState: InteractionUiState;
  focused: boolean;
  /** Deep-equality signature of every render input of the card. */
  sig: string;
};

export type ApprovalRows = {
  /** pending / answering / paused — the actionable queue. */
  queue: ApprovalRow[];
  /** expired / superseded — the 已离队 section. Settled rows never appear. */
  departed: ApprovalRow[];
};

export type ApprovalSource = {
  interactions: Interaction[];
  instances: Instance[];
  hosts: Host[];
  /** hub.answering: interaction ids POSTed locally, awaiting journal receipt. */
  answering: Record<string, true>;
  deviceId: string;
  workspaceLabel: (workspaceId: string) => string;
};

export type ApprovalFilters = {
  kind: string;
  hostId: string;
  workspaceId: string;
  focus: string | null;
};

const QUEUE_STATES = new Set<InteractionUiState>(["pending", "answering", "paused"]);

/**
 * Signature of the card's render inputs. `request` is immutable per
 * interaction on the wire but comes back as a fresh parse on every poll, so
 * it is included as JSON rather than compared by identity. The instance
 * fields are exactly those projectStatus(), the meta line and context read;
 * host fields drive hostName() and the paused projection.
 */
function rowSignature(
  item: Interaction,
  instance: Instance | undefined,
  host: Host | undefined,
  uiState: InteractionUiState,
  workspaceLabel: string,
  focused: boolean,
): string {
  return JSON.stringify({
    i: [
      item.state,
      item.kind,
      item.carrier,
      item.answerable,
      item.createdAt,
      item.updatedAt,
      item.deadline,
      item.answer,
      item.resolution,
      item.request,
    ],
    n: instance
      ? [
          instance.connectivity,
          instance.lifecycle,
          instance.activity,
          instance.workspaceId,
          instance.kind,
          instance.lastError,
          instance.usageRollup,
        ]
      : null,
    h: host ? [host.state, host.label] : null,
    w: workspaceLabel,
    u: uiState,
    f: focused ? 1 : 0,
  });
}

export function deriveApprovalRows(
  source: ApprovalSource,
  filters: ApprovalFilters,
): ApprovalRows {
  const instanceById = new Map(source.instances.map((instance) => [instance.id, instance]));
  const hostById = new Map(source.hosts.map((host) => [host.id, host]));

  const queue: ApprovalRow[] = [];
  const departed: ApprovalRow[] = [];

  for (const item of source.interactions) {
    // Cheap, join-free rejection first: kind/host filters need no join.
    if (filters.kind !== "all" && item.kind !== filters.kind) continue;
    if (filters.hostId && item.hostId !== filters.hostId) continue;

    // Settled on this device renders nowhere — skip projection and joins.
    if (settledOnThisDevice(item, source.deviceId)) continue;

    const instance = instanceById.get(item.instanceId);
    if (filters.workspaceId && instance?.workspaceId !== filters.workspaceId) continue;

    const host = hostById.get(item.hostId);
    const uiState = projectInteraction(item, {
      answering: Boolean(source.answering[item.id]),
      host,
      connectivity: instance?.connectivity,
      deviceId: source.deviceId,
    });
    if (uiState === "settled") continue;

    const focused = filters.focus === item.id;
    const row: ApprovalRow = {
      item,
      instance,
      host,
      uiState,
      focused,
      sig: rowSignature(
        item,
        instance,
        host,
        uiState,
        source.workspaceLabel(instance?.workspaceId ?? ""),
        focused,
      ),
    };
    if (QUEUE_STATES.has(uiState)) queue.push(row);
    else departed.push(row); // expired / superseded
  }

  return { queue, departed };
}

/* ------------------------------------------------------------------ */
/* Shared presentational labels for the single decision card          */
/* (ui-spec §2.5 facts <dl>, §2.2 approval card). Pure and unit-tested*/
/* so desktop and mobile never disagree about what a fact says.       */
/* ------------------------------------------------------------------ */

/**
 * Human label for the harness carrier the request arrived on. The wireframe
 * fact is 「来源 …」; a carrier the UI does not know (or `unsupported`) reads
 * 「未知」 rather than being invented (ui-spec §2.5).
 */
export const CARRIER_LABEL: Record<Interaction["carrier"], string> = {
  "claude-control": "Claude 控制面",
  "claude-hook": "Claude 钩子",
  "harness-hook": "工具钩子",
  "codex-rpc": "Codex",
  "acp-rpc": "ACP",
  "native-tty": "终端屏幕",
  unsupported: "未知",
};

export function carrierLabel(carrier: Interaction["carrier"]): string {
  return CARRIER_LABEL[carrier] ?? "未知";
}

/**
 * The 截止 fact: a known deadline renders as a clock; an unknown / absent
 * deadline is an em dash — never 「无限期」 and never 0 (ui-spec §2.2/§2.5).
 */
export function deadlineLabel(item: Interaction): string {
  return item.deadline.state === "known" ? formatClock(item.deadline.value) : "—";
}

/**
 * The status line text (ui-spec §2.5 state table). superseded keeps the
 * honest 「其它设备」 wording — the actor is another enrolled device, never a
 * specific platform name the data does not carry.
 */
export const QUEUE_STATUS_TEXT: Record<"pending" | "answering" | "paused", string> = {
  pending: "等待你的选择",
  answering: "已提交 · 等待确认",
  paused: "主机离线，交互暂停",
};

export const DEPARTED_STATUS_TEXT: Record<"expired" | "superseded", string> = {
  expired: "过期，未作用于新进程",
  superseded: "已在其它设备处理",
};

/** Shape-encoded status dot for the card's status line (visual-system §8.6). */
export function statusDotOf(uiState: InteractionUiState): UiStatus {
  switch (uiState) {
    case "answering":
      return "working";
    case "paused":
      return "unknown";
    case "expired":
      return "exited";
    case "superseded":
      return "idle";
    case "pending":
    default:
      return "blocked";
  }
}

/**
 * Request title for the card header (line 3 of the §2.5 card). The title is
 * the tool name for approvals, a carrier-aware label for terminal questions,
 * and the request's own title otherwise.
 */
export function decisionTitle(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.title;
  if (item.request.kind === "question") {
    return item.carrier === "native-tty" ? "终端提问" : item.request.title;
  }
  return item.request.title;
}

/**
 * Verbatim preview text (line 4). Approvals show the raw command/patch; a
 * native-tty question joins its field lines; an AskUserQuestion shows the
 * composed line. Never re-written.
 */
export function decisionPreview(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.description;
  if (item.request.kind === "question") {
    return item.carrier === "native-tty"
      ? item.request.fields.map((field) => field.description ?? field.title).join("\n")
      : `问你 ${item.request.fields.length} 题 · AskUserQuestion`;
  }
  if (item.request.kind === "plan-review") return item.request.title;
  return item.request.title;
}
