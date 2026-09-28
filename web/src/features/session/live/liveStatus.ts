/**
 * Screen-tier spinner status projection.
 *
 * The driver parses claude's status line (`· Razzmatazzing… (49m 38s · ↓ 66.0k
 * tokens · thinking with xhigh effort)`) and journals a `live.status` native
 * lifecycle at most once per distinct reading. Like `phase.ts` this is a pure
 * fold over observations: the latest reading wins, seq-ordered, and a
 * `liveStatus:"0"` tag clears it.
 *
 * Status only (design §2.4): the spinner verb, token estimate and phrase are
 * the TUI's own chrome, never message content.
 */
import type { Observation } from "../../../types/generated";
import { nativeLifecycle } from "./payloadGuard";

const TAGS: Record<string, string> = {
  live: "liveStatus",
  verb: "verb",
  phrase: "phrase",
  tokensLabel: "tokensLabel",
  tokensDown: "tokensDown",
  elapsedScreen: "elapsedScreen",
  since: "since",
  interruptible: "interruptible",
};

/** The latest screen-tier spinner reading, or `null` when none is live. */
export type ScreenLiveStatus = {
  /** A `liveStatus:"0"` clear has been folded (no spinner on screen). */
  active: boolean;
  verb: string | null;
  phrase: string | null;
  tokensLabel: string | null;
  tokensDown: number | null;
  /** Elapsed exactly as the TUI printed it. */
  elapsedScreen: string | null;
  /** RFC3339 re-anchor (`now − the screen's printed elapsed`). */
  since: string | null;
  interruptible: boolean;
  observedAt: string | null;
};

type TurnLifecycle = Extract<Observation, { kind: "lifecycle" }>;

function isStatusEvent(ev: Observation): ev is TurnLifecycle {
  return nativeLifecycle(ev)?.nativeName === "live.status";
}

function seqOf(ev: Observation): bigint {
  try {
    return BigInt(ev.seq);
  } catch {
    return 0n;
  }
}

/**
 * `relatedIds` is untrusted wire data: it may be missing, a scalar, an array,
 * or carry non-string tag values (`{phrase: 42}` reached
 * `phrase.toLowerCase()` and crashed the whole strip). Only a plain record is
 * consumed; anything else reads as an empty tag bag.
 */
function tagRecord(value: unknown): Record<string, unknown> {
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    return value as Record<string, unknown>;
  }
  return {};
}

/** A tag only survives as a string; a number/object/null/array tag is dropped. */
function strTag(tags: Record<string, unknown>, key: string): string | null {
  const value = tags[key];
  return typeof value === "string" ? value : null;
}

export function liveStatus(events: readonly Observation[]): ScreenLiveStatus | null {
  let latest: TurnLifecycle | null = null;
  for (const ev of events) {
    if (!isStatusEvent(ev)) continue;
    if (!latest || seqOf(ev) >= seqOf(latest)) latest = ev;
  }
  if (!latest) return null;
  const payload = nativeLifecycle(latest);
  if (!payload) return null;
  const tags = tagRecord(payload.relatedIds);
  // A missing/non-string live tag keeps the historical default (active):
  // only the literal "0" spelling clears the spinner.
  const active = strTag(tags, TAGS.live) !== "0";
  if (!active) {
    return {
      active: false,
      verb: null,
      phrase: null,
      tokensLabel: null,
      tokensDown: null,
      elapsedScreen: null,
      since: null,
      interruptible: false,
      observedAt: latest.observedAt,
    };
  }
  const down = strTag(tags, TAGS.tokensDown);
  const downCount = down != null && down.trim() !== "" && Number.isFinite(Number(down)) ? Number(down) : null;
  return {
    active: true,
    verb: strTag(tags, TAGS.verb),
    phrase: strTag(tags, TAGS.phrase),
    tokensLabel: strTag(tags, TAGS.tokensLabel),
    tokensDown: downCount,
    elapsedScreen: strTag(tags, TAGS.elapsedScreen),
    since: strTag(tags, TAGS.since),
    interruptible: strTag(tags, TAGS.interruptible) === "1",
    observedAt: latest.observedAt,
  };
}

/**
 * Whether the phrase marks the reasoning part of the turn. The random spinner
 * verb carries no information; `thinking with xhigh effort` / `thought for 9s`
 * do. Mirrors the Rust `is_thinking_phrase` rule.
 */
export function phraseIsThinking(phrase: unknown): boolean {
  // Defense in depth: the projection already drops non-string tags, but a
  // caller with a raw/untrusted phrase (`42.toLowerCase()` crashed the strip)
  // must never reach a string method.
  if (typeof phrase !== "string" || phrase.length === 0) return false;
  const lower = phrase.toLowerCase();
  return lower.includes("thinking") || lower.includes("thought");
}
