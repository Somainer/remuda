import type { Command } from "../types/command";
import type { ContentStatus } from "../types/generated";
import type { Connectivity, Host, Lifecycle } from "../types/instance";
import type { Interaction } from "../types/interaction";
import type { CapabilitySnapshot } from "../types/nativeRef";
import { provisionOf } from "./capabilities";
import { nativeCleared as nativeClearedFact, projectInteraction } from "./interactionStatus";
import type { OutboxState } from "./outbox";

/**
 * P0-3 display vocabulary: a pure projection of facts the backend already
 * reports onto the eight rows of `docs/design/workbench-ux-exploration.md`
 * §5 P0-3, mapped field-by-field in `workbench-ux-plan.md` §3.
 *
 * This is a **display rule set, not a state machine**. Nothing here invents a
 * phase, interpolates between rows, or advances on a timer: every row is
 * decided by fields that exist on `Command`, `Instance`, `Interaction`, the
 * capability snapshot, or the `DELETE` response. Two invariants hold for every
 * input, including inputs this module does not recognise:
 *
 * 1. **Unknown never renders as success.** `success` is true only for
 *    {@link ROW_TURN_ENDED}, and only when a native turn-completion fact
 *    proves it. Absent or contradictory facts fall through to
 *    "状态待确认" — never to a finished-looking row.
 * 2. **No row offers a re-send.** {@link CommandStatusAction} has no resend
 *    member, so "refresh" can only re-read. Recovering from an unconfirmed
 *    send is a human decision, not something a status chip may trigger
 *    (exploration §5 P0-3: 刷新不产生新发送).
 */
export type CommandStatusKey =
  | "awaiting-send"
  | "accepted"
  | "sent-awaiting-ack"
  | "pending-offline"
  | "send-rejected"
  | "unconfirmed"
  | "needs-answer"
  | "answer-submitted"
  | "turn-ended"
  | "record-deleted-purge-pending";

/**
 * Actions a row may offer. Deliberately closed, and deliberately missing a
 * resend/retry member: the contract allows re-reading and re-observing only.
 */
export type CommandStatusAction =
  | "view"
  | "cancel-unsent"
  | "refresh"
  | "open-interaction"
  | "copy-diagnostic";

/**
 * Visual weight. Separate from {@link CommandStatusRow.success} so a skin can
 * style "已受理" as progress without it ever reading as a finished task.
 */
export type CommandStatusTone =
  | "queued"
  | "accepted"
  | "in-flight"
  | "unknown"
  | "attention"
  | "done"
  | "removed";

export type CommandStatusRow = {
  key: CommandStatusKey;
  /** The single Chinese phrase list / tab / detail all render. */
  label: string;
  /** True only with a native turn-completion fact. Never inferred. */
  success: boolean;
  tone: CommandStatusTone;
  actions: readonly CommandStatusAction[];
};

/**
 * The status vocabulary, exported so list / tab / detail converge on one
 * wording instead of three. Batches D–G read these constants rather than
 * writing their own strings.
 */
export const COMMAND_STATUS_LABEL: Record<CommandStatusKey, string> = {
  "awaiting-send": "等待发送",
  accepted: "已受理",
  "sent-awaiting-ack": "已发送，等待确认",
  "pending-offline": "待发送（离线）",
  "send-rejected": "未送达",
  unconfirmed: "状态待确认",
  "needs-answer": "需要你回答",
  "answer-submitted": "回答已提交，等待处理",
  "turn-ended": "本轮已结束",
  "record-deleted-purge-pending": "会话记录已删除，主机数据待清理",
};

/**
 * Same row as {@link COMMAND_STATUS_LABEL}.accepted, worded for the instance
 * side of "已受理 / 会话已创建". Still not a success — the session exists, the
 * work has not started.
 */
export const CREATED_LABEL = "会话已创建";

/**
 * `PromptMode` wording (plan §3). `new-turn` is an ordinary send and carries
 * no badge; the other two must be visible because they change when the text
 * reaches the agent.
 */
export const PROMPT_MODE_LABEL = {
  steer: "引导",
  queue: "排队",
} as const;

/** Shown next to an emulated capability; D-028 §6 requires it be user-visible. */
export const EMULATED_LABEL = "Remuda 代发";

function row(
  key: CommandStatusKey,
  tone: CommandStatusTone,
  actions: readonly CommandStatusAction[],
  success = false,
): CommandStatusRow {
  return { key, label: COMMAND_STATUS_LABEL[key], success, tone, actions };
}

const ROW_AWAITING_SEND = row("awaiting-send", "queued", ["view", "cancel-unsent"]);
const ROW_ACCEPTED = row("accepted", "accepted", ["view"]);
const ROW_PENDING_OFFLINE = row("pending-offline", "queued", ["view", "cancel-unsent"]);
const ROW_SEND_REJECTED = row("send-rejected", "attention", ["copy-diagnostic"]);
const ROW_SENT_AWAITING_ACK = row("sent-awaiting-ack", "in-flight", ["view"]);
const ROW_UNCONFIRMED = row("unconfirmed", "unknown", ["refresh", "copy-diagnostic"]);
const ROW_NEEDS_ANSWER = row("needs-answer", "attention", ["open-interaction"]);
const ROW_ANSWER_SUBMITTED = row("answer-submitted", "in-flight", ["view"]);
const ROW_TURN_ENDED = row("turn-ended", "done", ["view"], true);
const ROW_DELETED = row("record-deleted-purge-pending", "removed", ["view", "copy-diagnostic"]);

/**
 * Facts to project. Every field is optional: a caller supplies what it has,
 * and supplying nothing yields "状态待确认" rather than a guess.
 */
export type CommandStatusFacts = {
  /** Server-side command record, when one exists. */
  command?: Pick<Command, "state" | "dispatch" | "resolution"> | null;
  /**
   * Whether `command` is backed by a **server-assigned** `commandId`.
   * Today the store seeds a local `local_…` id before the response lands
   * (`lib/store.ts:521-527`); such a bubble has no server identity, must not
   * be used to query `/v1/commands`, and must not read as delivered. Pass
   * `false` and this projection degrades to "状态待确认" unless the bubble is
   * still plainly queued. C2 replaces this with `clientRequestId` +
   * `commandId: Id | null`.
   */
  hasServerCommandId?: boolean;
  /** Optimistic local bubble state, before/without a server command. */
  localState?: Command["state"] | "unknown";
  /**
   * D-055 durable outbox state for this bubble (present when it is backed by
   * an outbox row), plus whether the link is currently non-live. Together
   * these distinguish an offline-queued message (待发送（离线）) from an
   * ordinary queued send and a definite rejection (未送达).
   */
  outboxState?: OutboxState;
  offline?: boolean;
  instance?: {
    lifecycle?: Lifecycle;
    connectivity?: Connectivity;
    lastError?: string | null;
  } | null;
  interaction?: {
    interaction: Interaction;
    /** Local "submitting" flag, as `projectInteraction` understands it. */
    answering?: boolean;
    deviceId?: string;
  } | null;
  host?: Host;
  /** Turn-completion evidence; only a native fact may close a turn. */
  turn?: {
    contentStatus?: ContentStatus;
    capabilities?: CapabilitySnapshot | null;
  } | null;
  /** `DELETE /v1/instances/{id}` outcome. The Hub record is gone either way. */
  deletion?: { nodePurge?: string | null } | null;
};

/**
 * `Instance.lastError` value the Hub sets when the Node reconnected under a
 * new epoch: everything observed before it is unproven, not false.
 */
export const NODE_EPOCH_CHANGED = "node-epoch-changed";

/** Row 4 facts on the command itself. */
function commandUnconfirmed(command: CommandStatusFacts["command"]): boolean {
  if (!command) return false;
  return command.resolution === "unknown" || command.resolution === "reconciling";
}

/** Row 4 facts on the instance. */
function instanceUnconfirmed(instance: CommandStatusFacts["instance"]): boolean {
  if (!instance) return false;
  if (instance.connectivity === "disconnected" || instance.connectivity === "reconciling") return true;
  if (instance.lifecycle === "unknown" || instance.lifecycle === "reconciling") return true;
  return instance.lastError === NODE_EPOCH_CHANGED;
}

/**
 * Row 7. `complete` / `interrupted` alone prove only that a *content stream*
 * stopped. They close a turn only when the session actually reports native
 * turn boundaries — otherwise Remuda inferred the boundary and the honest
 * answer is "状态待确认" (plan §3; never terminal idle, never `Settled`).
 */
function turnEnded(turn: CommandStatusFacts["turn"]): boolean {
  if (!turn) return false;
  if (turn.contentStatus !== "complete" && turn.contentStatus !== "interrupted") return false;
  const capability = turn.capabilities?.capabilities?.["completion-native-turn"];
  return capability?.state === "supported" && provisionOf(capability) === "native";
}

/**
 * Project facts onto one display row.
 *
 * Precedence, most-committing fact first — a higher rule only wins when its
 * own fields are actually present, so this stays a lookup rather than a
 * pipeline every command walks:
 *
 * 1. the record was deleted (nothing below it is still about a live session);
 * 2. a pending interaction is blocking a human (the only actionable row);
 * 3. an answer is committed and the native side has not cleared it;
 * 4. any unknown / reconciling / disconnected / epoch-changed fact;
 * 5. a native turn-completion fact;
 * 6. written to transport with a clear resolution;
 * 7. accepted, or an instance still being created;
 * 8. queued and not yet dispatched.
 *
 * Rule 4 sits above rules 5–8 on purpose: that ordering is what makes
 * "unknown never renders as success" true rather than aspirational.
 */
export function projectCommandStatus(facts: CommandStatusFacts): CommandStatusRow {
  // 1 — Hub record deleted with the Node's copy unconfirmed. A *purged*
  // deletion is not handled here: see {@link projectDeletion}.
  if (facts.deletion && facts.deletion.nodePurge !== "purged") return ROW_DELETED;

  // 2 & 3 — interaction rows, via the existing UI projection so host offline
  // and other-device answers keep the meanings they already have.
  const pending = facts.interaction;
  if (pending) {
    const ui = projectInteraction(pending.interaction, {
      answering: pending.answering,
      host: facts.host,
      connectivity: facts.instance?.connectivity,
      deviceId: pending.deviceId,
    });
    const cleared = nativeClearedFact(pending.interaction);
    if (ui === "pending" && pending.interaction.answerable) return ROW_NEEDS_ANSWER;
    if (!cleared && (ui === "settled" || ui === "superseded" || ui === "answering")) {
      // An answer whose own delivery is unknown is not a submitted answer.
      if (pending.interaction.delivery === "unknown") return ROW_UNCONFIRMED;
      return ROW_ANSWER_SUBMITTED;
    }
    if (ui === "paused" || pending.interaction.state === "unknown") return ROW_UNCONFIRMED;
    // Natively cleared: the interaction stops deciding the row, but it proves
    // nothing about the turn either. Fall through to rules 4-8.
  }

  // 4 — anything unproven outranks every optimistic row below. The D-055
  // outbox states are decided locally and narrow the old fallback:
  if (facts.outboxState === "rejected") return ROW_SEND_REJECTED;
  if (facts.outboxState === "unknown") return ROW_UNCONFIRMED;
  // "sent" reached the Hub/Node (accepted or forwarded) and "done" is
  // journal-confirmed: project as delivered, never 状态待确认, while the
  // journal join is awaited (item 13).
  if (facts.outboxState === "sent") return ROW_SENT_AWAITING_ACK;
  if (facts.outboxState === "done") return ROW_ACCEPTED;
  if (facts.outboxState === "held") return ROW_AWAITING_SEND;
  if (facts.outboxState === "inflight") return ROW_SENT_AWAITING_ACK;
  if (facts.outboxState === "pending" && facts.offline) return ROW_PENDING_OFFLINE;
  if (instanceUnconfirmed(facts.instance)) return ROW_UNCONFIRMED;
  if (commandUnconfirmed(facts.command)) return ROW_UNCONFIRMED;
  // No server identity yet: only a plainly-queued bubble may claim a phase.
  if (facts.hasServerCommandId === false && facts.localState !== "queued") return ROW_UNCONFIRMED;

  // 5 — the only row allowed to read as success.
  if (turnEnded(facts.turn)) return ROW_TURN_ENDED;

  // 6 — written to transport, resolution clear.
  const command = facts.command;
  if (command?.dispatch === "transport-written" && command.resolution === "clear") {
    return ROW_SENT_AWAITING_ACK;
  }
  if (command?.dispatch === "native-acknowledged" && command.resolution === "clear") {
    // Acknowledged transport is still not a finished task; §5 P0-3 forbids
    // showing business success from a command reaching the harness.
    return ROW_ACCEPTED;
  }

  // 7 — accepted, or an instance mid-creation.
  const lifecycle = facts.instance?.lifecycle;
  if (command?.state === "accepted" || command?.state === "settled") return ROW_ACCEPTED;
  if (lifecycle === "requested" || lifecycle === "preparing" || lifecycle === "starting") {
    return ROW_ACCEPTED;
  }

  // 8 — queued, not yet dispatched.
  const queued =
    (command?.state === "queued" &&
      (command.dispatch === "not-dispatched" || command.dispatch === "intent-durable")) ||
    facts.localState === "queued";
  if (queued) return ROW_AWAITING_SEND;

  // Everything else, including facts this module does not recognise.
  return ROW_UNCONFIRMED;
}

/**
 * Label for the "已受理 / 会话已创建" row. The instance wording is used when
 * the only accepted fact is that a session is being created.
 */
export function acceptedLabel(facts: Pick<CommandStatusFacts, "command" | "instance">): string {
  if (!facts.command && facts.instance) return CREATED_LABEL;
  return COMMAND_STATUS_LABEL.accepted;
}

/**
 * Row 8 on its own. `DELETE /v1/instances/{id}` removes the Hub record
 * whatever `nodePurge` says, so this never reports failure-to-delete; it
 * reports whether the *host's* copy is confirmed gone.
 *
 * `purged` is the only confirming value, and it returns `null` rather than a
 * row: a confirmed clean delete has no lingering status to display. Anything
 * else — `node-offline`, `node-rejected`, `purge-failed`, an absent field, a
 * value this client has never heard of — keeps the standing notice, because
 * "not confirmed purged" is the fact and silence would overstate it.
 */
export function projectDeletion(result: { nodePurge?: string | null } | null | undefined): CommandStatusRow | null {
  if (!result) return null;
  return result.nodePurge === "purged" ? null : ROW_DELETED;
}

/**
 * Re-exported so a component rendering the "需要你回答" / "回答已提交" rows can
 * get the row and its submittability from one import. Defined next to
 * `projectInteraction`, whose rules it depends on.
 */
export { canSubmitAnswer, answerPendingNative, nativeCleared } from "./interactionStatus";
