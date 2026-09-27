/**
 * The one streamed token count the strip may show.
 *
 * Preference (design: prefer hook/transcript usage, fall back to the screen):
 * a real `usage` observation from this turn wins; until it lands (~seconds
 * late) the screen spinner estimate is shown. The two never appear together.
 */
import type { Observation } from "../../../types/generated";
import { nativeLifecycle, usagePayload } from "./payloadGuard";

function seqOf(ev: Observation): bigint {
  try {
    return BigInt(ev.seq);
  } catch {
    return 0n;
  }
}

/** Seq of the last turn-start phase tag, so usage from a previous turn never
 * leaks into the current reading. */
function lastTurnStartSeq(events: readonly Observation[]): bigint | null {
  let start: bigint | null = null;
  for (const ev of events) {
    const payload = nativeLifecycle(ev);
    if (!payload) continue;
    const tags = (payload.relatedIds ?? {}) as Record<string, unknown>;
    if (tags.phase === "prompt-accepted") start = seqOf(ev);
  }
  return start;
}

/** Latest authoritative output-token usage observed for the current turn. */
export function usageOutputTokens(events: readonly Observation[]): number | null {
  const start = lastTurnStartSeq(events);
  let count: bigint | null = null;
  let latest: bigint | null = null;
  for (const ev of events) {
    if (ev.kind !== "usage") continue;
    if (start != null && seqOf(ev) < start) continue;
    const payload = usagePayload(ev);
    if (!payload) continue;
    const scope = payload.scope;
    if (scope !== "message" && scope !== "turn") continue;
    const output = payload.outputTokens as
      | { state?: string; value?: unknown }
      | null
      | undefined;
    if (!output || output.state !== "known") continue;
    let value: bigint;
    try {
      value = BigInt(String(output.value));
    } catch {
      continue;
    }
    // Keep the newest reading (seq, not encounter order — gap backfill);
    // per-message snapshots replace, they do not add.
    const here = seqOf(ev);
    if (latest == null || here >= latest) {
      latest = here;
      count = value;
    }
  }
  return count == null ? null : Number(count);
}
