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
  return (
    ev.kind === "lifecycle" && ev.payload.type === "native" && ev.payload.nativeName === "live.status"
  );
}

function seqOf(ev: Observation): bigint {
  try {
    return BigInt(ev.seq);
  } catch {
    return 0n;
  }
}

export function liveStatus(events: readonly Observation[]): ScreenLiveStatus | null {
  let latest: TurnLifecycle | null = null;
  for (const ev of events) {
    if (!isStatusEvent(ev)) continue;
    if (!latest || seqOf(ev) >= seqOf(latest)) latest = ev;
  }
  if (!latest || latest.payload.type !== "native") return null;
  const tags = latest.payload.relatedIds ?? {};
  const active = tags[TAGS.live] !== "0";
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
  const down = tags[TAGS.tokensDown];
  return {
    active: true,
    verb: tags[TAGS.verb] ?? null,
    phrase: tags[TAGS.phrase] ?? null,
    tokensLabel: tags[TAGS.tokensLabel] ?? null,
    tokensDown: down != null && Number.isFinite(Number(down)) ? Number(down) : null,
    elapsedScreen: tags[TAGS.elapsedScreen] ?? null,
    since: tags[TAGS.since] ?? null,
    interruptible: tags[TAGS.interruptible] === "1",
    observedAt: latest.observedAt,
  };
}

/**
 * Whether the phrase marks the reasoning part of the turn. The random spinner
 * verb carries no information; `thinking with xhigh effort` / `thought for 9s`
 * do. Mirrors the Rust `is_thinking_phrase` rule.
 */
export function phraseIsThinking(phrase: string | null | undefined): boolean {
  if (!phrase) return false;
  const lower = phrase.toLowerCase();
  return lower.includes("thinking") || lower.includes("thought");
}
