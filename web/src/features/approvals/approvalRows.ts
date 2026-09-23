import {
  projectInteraction,
  settledOnThisDevice,
  type InteractionUiState,
} from "../../lib/interactionStatus";
import type { Host, Instance } from "../../types/instance";
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
