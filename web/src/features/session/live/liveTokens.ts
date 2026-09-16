/**
 * The one streamed token count the strip may show.
 *
 * Preference (design: prefer hook/transcript usage, fall back to the screen):
 * a real `usage` observation from this turn wins; until it lands (~seconds
 * late) the screen spinner estimate is shown. The two never appear together.
 */
import type { Observation } from "../../../types/generated";

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
    if (ev.kind !== "lifecycle" || ev.payload.type !== "native") continue;
    if (ev.payload.relatedIds?.phase === "prompt-accepted") start = seqOf(ev);
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
    const scope = ev.payload.scope;
    if (scope !== "message" && scope !== "turn") continue;
    const output = ev.payload.outputTokens;
    if (output.state !== "known") continue;
    // Keep the newest reading (seq, not encounter order — gap backfill);
    // per-message snapshots replace, they do not add.
    const here = seqOf(ev);
    if (latest == null || here >= latest) {
      latest = here;
      count = BigInt(output.value);
    }
  }
  return count == null ? null : Number(count);
}
