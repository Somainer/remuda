import type { FleetBroadcastBody, FleetBroadcastEntry, FleetBroadcastResult } from "../../lib/api";
import type { components } from "../../lib/api.generated";

/** What the broadcast box sends: a prompt or a logical key. */
export type BroadcastMode = "prompt" | "key";

/** Keys the box offers; the Hub validates the name again server-side. */
export const BROADCAST_KEYS = ["enter", "esc", "ctrl+c"] as const;
export type BroadcastKey = (typeof BROADCAST_KEYS)[number];

/** Filter state of the box. Empty strings mean "no filter". */
export type BroadcastFilter = {
  hostId: string;
  kind: string;
};

/** Form state the box turns into a /v1/fleet/broadcast body. */
export type BroadcastForm = {
  mode: BroadcastMode;
  text: string;
  key: BroadcastKey;
  filter: BroadcastFilter;
};

/** PTY bytes for a logical key, matching the CLI's `encode_keys`. */
const KEY_BYTES: Record<BroadcastKey, string> = {
  enter: "\r",
  esc: "\x1b",
  "ctrl+c": "\x03",
};

function base64(text: string): string {
  const bytes = Array.from(text, (ch) => ch.charCodeAt(0) & 0xff);
  return btoa(String.fromCharCode(...bytes));
}

/**
 * Build the `/v1/fleet/broadcast` body. Selection is always `all` plus the
 * optional host/kind narrowing, so the Hub never sees an empty selection.
 * Returns a reason string instead of a body when the form is not sendable.
 */
export function buildBroadcastBody(form: BroadcastForm): { body: FleetBroadcastBody } | { error: string } {
  const filters: Pick<FleetBroadcastBody, "hosts" | "kinds"> = {};
  if (form.filter.hostId) filters.hosts = [form.filter.hostId];
  if (form.filter.kind) filters.kinds = [form.filter.kind];

  if (form.mode === "prompt") {
    const text = form.text.trim();
    if (!text) return { error: "输入要群发的内容" };
    return {
      body: {
        all: true,
        confirm: false,
        ...filters,
        operation: "instance.send",
        payload: {
          input: { type: "prompt", mode: "new-turn", blocks: [{ type: "text", text }], origin: "ui" },
          completionScope: "native-turn",
        },
      },
    };
  }
  return {
    body: {
      all: true,
      confirm: false,
      ...filters,
      operation: "tty.write",
      payload: { keys: [form.key], dataBase64: base64(KEY_BYTES[form.key]), source: "ui" },
    },
  };
}

/**
 * Ledger acceptance line: what the Hub accepted at POST time, before any
 * Node settlement. The authoritative outcome is rendered per row.
 */
export function summarize(result: FleetBroadcastResult): string {
  const accepted = result.accepted ?? 0;
  const failed = result.failed ?? 0;
  const skipped = result.skipped ?? 0;
  return `已接受 ${accepted} · 失败 ${failed} · 跳过 ${skipped}`;
}

/** Failed entries first so a partial failure is visible without scrolling. */
export function orderResults(result: FleetBroadcastResult): FleetBroadcastEntry[] {
  const entries = result.results ?? [];
  return [...entries].sort((a, b) => Number(a.ok ?? false) - Number(b.ok ?? false));
}

/**
 * Authoritative delivery state of one broadcast result row.
 *
 * `ok: true` only means the Hub accepted the command — the fleet JSON never
 * carries the settlement, so green confirmation is impossible from this row
 * alone (D-053 item 2). The authoritative outcome is read from
 * `GET /v1/instances/{id}/commands/{commandId}` → `settlement.outcome`.
 */
export type DeliveryState = "queued" | "forwarded" | "confirmed" | "cancelled" | "expired" | "replayed" | "failed";

/**
 * The provisional state straight from the fan-out row. A settled row here is
 * not confirmed: the fleet projection omits `settlement`, so `state=settled`
 * only says the ledger is closed — the outcome still has to be read from the
 * command resource.
 */
export function provisionalState(entry: FleetBroadcastEntry): DeliveryState {
  if (!entry.ok) return "failed";
  if (entry.replayed) return "replayed";
  return entry.forwarded ? "forwarded" : "queued";
}

type CommandRecord = components["schemas"]["CommandRecord"];

/**
 * Terminal outcomes: every settlement outcome is a final label. `expired`
 * (a token/profile TTL refusal) is a neutral terminal state, not a send.
 * Any outcome we do not have a specific label for still terminates the row
 * as a neutral 已结束 rather than masquerading as 已发送/已排队.
 */
export function classifyCommand(command: CommandRecord): DeliveryState {
  const outcome = command.settlement?.outcome;
  if (command.state === "settled") {
    if (outcome === "completed") return "confirmed";
    if (outcome === "rejected") return "failed";
    if (outcome === "cancelled") return "cancelled";
    if (outcome === "expired") return "expired";
    // Settled with an outcome we do not model: terminal, never a success and
    // never an open send.
    return "cancelled";
  }
  return command.forwarded ? "forwarded" : "queued";
}

export const SETTLED_STATES: ReadonlySet<DeliveryState> = new Set([
  "confirmed",
  "failed",
  "cancelled",
  "expired",
]);

export const DELIVERY_LABEL: Record<DeliveryState, string> = {
  queued: "已排队",
  forwarded: "已发送",
  confirmed: "已确认",
  cancelled: "已结束",
  expired: "已过期",
  replayed: "重放",
  failed: "失败",
};

/** Per-request read deadline, so one hung GET cannot defeat the overall cap. */
const READ_TIMEOUT_MS = 3_000;
/** One follow-up read for a row still open after the immediate read. */
const FOLLOW_UP_DELAY_MS = 800;
const FOLLOW_UP_TIMEOUT_MS = 6_000;

/** One bounded GET: an AbortController enforces the deadline per request. */
async function boundedRead(
  read: (signal: AbortSignal) => Promise<CommandRecord>,
  timeoutMs: number,
): Promise<CommandRecord> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new Error("command status read timed out")), timeoutMs);
  try {
    return await read(controller.signal);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Enrich one accepted broadcast row with the authoritative command outcome.
 *
 * Exactly two reads: one immediate, and ONE follow-up for a row that is still
 * open (a freshly forwarded send usually settles within a second). Each GET
 * has its own AbortController deadline, so a hung request can never defeat
 * the cap. A failed follow-up read keeps the last authoritative state (the
 * immediate read) instead of downgrading it.
 */
export async function resolveEntry(
  entry: FleetBroadcastEntry,
  read: (instanceId: string, commandId: string, signal: AbortSignal) => Promise<CommandRecord>,
): Promise<{ state: DeliveryState; reason?: string }> {
  const provisional = provisionalState(entry);
  if (provisional === "failed" || provisional === "replayed") return { state: provisional };

  const instanceId = entry.instanceId;
  const commandId = entry.commandId;
  if (!instanceId || !commandId) return { state: provisional };

  // Immediate read. A failure here leaves the provisional (accepted) label.
  let command: CommandRecord;
  try {
    command = await boundedRead((signal) => read(instanceId, commandId, signal), READ_TIMEOUT_MS);
  } catch {
    return { state: provisional };
  }
  let state = classifyCommand(command);
  if (SETTLED_STATES.has(state)) {
    return { state, reason: command.settlement?.reason ?? undefined };
  }

  // ONE bounded follow-up for the still-open row.
  try {
    await new Promise((resolve) => setTimeout(resolve, FOLLOW_UP_DELAY_MS));
    command = await boundedRead((signal) => read(instanceId, commandId, signal), FOLLOW_UP_TIMEOUT_MS);
    const next = classifyCommand(command);
    // Keep the last authoritative/known state; only overwrite when the new
    // read is itself terminal, otherwise preserve the immediate read.
    if (SETTLED_STATES.has(next)) {
      state = next;
      return { state, reason: command.settlement?.reason ?? undefined };
    }
  } catch {
    // Hung/failed follow-up: keep the state from the immediate read rather
    // than downgrading a forwarded row.
  }
  return { state };
}

/** Aggregate authoritative row states into fan-out summary counts. */
export function summarizeRows(
  states: DeliveryState[],
): { confirmed: number; failed: number; pending: number; cancelled: number } {
  const out = { confirmed: 0, failed: 0, pending: 0, cancelled: 0 };
  for (const state of states) {
    if (state === "confirmed") out.confirmed += 1;
    else if (state === "failed") out.failed += 1;
    else if (state === "cancelled" || state === "expired") out.cancelled += 1;
    else out.pending += 1;
  }
  return out;
}
