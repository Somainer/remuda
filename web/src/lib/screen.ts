import type { Observation } from "../types/observation";

export type ScreenRead = {
  lines: string[];
};

const DONE_LINE = /^\s*DONE(?:\s|$)/;

export function lastLines(lines: string[], n = 3): string[] {
  return lines.map((line) => line.replace(/\s+$/g, "")).filter((line) => line.length > 0).slice(-n);
}

export function doneFromLines(lines: string[]): boolean {
  return lines.some((line) => DONE_LINE.test(line));
}

export function parseScreenBody(body: unknown): ScreenRead {
  if (!body || typeof body !== "object") return { lines: [] };
  const rec = body as { lines?: unknown; text?: unknown };
  if (Array.isArray(rec.lines)) {
    return { lines: rec.lines.filter((line): line is string => typeof line === "string") };
  }
  if (typeof rec.text === "string") {
    return { lines: rec.text.split(/\r?\n/) };
  }
  return { lines: [] };
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

function knowledgeText(value: unknown): string | null {
  if (typeof value === "string") return value;
  const rec = asRecord(value);
  if (!rec) return null;
  if (typeof rec.value === "string") return rec.value;
  return null;
}

function payloadOf(obs: Observation): Record<string, unknown> | null {
  return asRecord(obs.payload) ?? asRecord(obs);
}

export function isScreenObservation(obs: Observation): boolean {
  const payload = payloadOf(obs);
  const name = typeof payload?.nativeName === "string" ? payload.nativeName : "";
  if (name === "prompt_echo" || name === "line-matcher" || name === "agent_status") return false;
  if (name === "screen") return true;
  return obs.kind === "raw_tty";
}

export function screenTextOf(obs: Observation): string {
  const payload = payloadOf(obs);
  if (!payload) return "";
  return (
    knowledgeText(payload.status) ??
    (typeof payload.text === "string" ? payload.text : null) ??
    (typeof payload.output === "string" ? payload.output : null) ??
    ""
  );
}

/**
 * Latest screen-derived snapshot from a followed journal, with the seq of the
 * observation it came from. The seq orders journal-derived screens against
 * in-flight `tty.screen` RPC reads so a read that started before a newer
 * journal frame cannot overwrite it on its stale resolution.
 */
export function latestScreenSnapshot(events: Observation[]): { lines: string[]; seq: string | null } {
  for (let i = events.length - 1; i >= 0; i--) {
    const obs = events[i];
    if (!isScreenObservation(obs)) continue;
    const text = screenTextOf(obs);
    if (!text) continue;
    return { lines: text.split(/\r?\n/), seq: obs.seq };
  }
  return { lines: [], seq: null };
}

/** Latest screen-derived snapshot from a followed journal. */
export function latestScreenFromObservations(events: Observation[]): ScreenRead {
  const { lines } = latestScreenSnapshot(events);
  return { lines };
}
