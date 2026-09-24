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

/** One-line summary of a fan-out, e.g. `已接受 3 · 失败 0 · 跳过 1`. */
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
export type DeliveryState = "queued" | "forwarded" | "confirmed" | "cancelled" | "replayed" | "failed";

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

/** Classify an authoritative command ledger row. */
export function classifyCommand(command: CommandRecord): DeliveryState {
  const outcome = command.settlement?.outcome;
  if (command.state === "settled") {
    if (outcome === "completed") return "confirmed";
    if (outcome === "rejected") return "failed";
    if (outcome === "cancelled") return "cancelled";
    // Settled without an outcome we can vouch for is not a success.
    return command.forwarded ? "forwarded" : "queued";
  }
  return command.forwarded ? "forwarded" : "queued";
}

export const SETTLED_STATES: ReadonlySet<DeliveryState> = new Set(["confirmed", "failed", "cancelled"]);

export const DELIVERY_LABEL: Record<DeliveryState, string> = {
  queued: "已排队",
  forwarded: "已发送",
  confirmed: "已确认",
  cancelled: "已取消",
  replayed: "重放",
  failed: "失败",
};

/**
 * Enrich one accepted broadcast row with the authoritative command outcome.
 *
 * One read per row, plus bounded follow-up reads while the command is still
 * open (a freshly forwarded send often settles a few hundred ms later).
 * Stops after `deadlineMs` (~10s); anything still open stays at its
 * provisional 已排队 / 已发送 label rather than being guessed as confirmed.
 */
export async function resolveEntry(
  entry: FleetBroadcastEntry,
  read: (instanceId: string, commandId: string) => Promise<CommandRecord>,
  deadlineMs = 10_000,
): Promise<{ state: DeliveryState; reason?: string }> {
  const provisional = provisionalState(entry);
  if (provisional === "failed" || provisional === "replayed") return { state: provisional };

  const instanceId = entry.instanceId;
  const commandId = entry.commandId;
  if (!instanceId || !commandId) return { state: provisional };

  const deadline = Date.now() + deadlineMs;
  for (;;) {
    let command: CommandRecord;
    try {
      command = await read(instanceId, commandId);
    } catch {
      // A read failure must not fabricate a result: leave the provisional.
      return { state: provisional };
    }
    const state = classifyCommand(command);
    if (SETTLED_STATES.has(state)) {
      return { state, reason: command.settlement?.reason ?? undefined };
    }
    if (Date.now() >= deadline) return { state };
    await new Promise((resolve) => setTimeout(resolve, 400));
  }
}
