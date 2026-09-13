import type { FleetBroadcastBody, FleetBroadcastEntry, FleetBroadcastResult } from "../../lib/api";

/** What the broadcast box sends: a prompt or a logical key. */
export type BroadcastMode = "prompt" | "key";

/** Keys the box offers; the Hub validates the name again server-side. */
export const BROADCAST_KEYS = ["enter", "esc", "ctrl+c"] as const;
export type BroadcastKey = (typeof BROADCAST_KEYS)[number];

/** Filter state of the broadcast box. Empty strings mean "no filter". */
export type BroadcastFilter = {
  hostId: string;
  kind: string;
};

/** Form state the box turns into a request body. */
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
