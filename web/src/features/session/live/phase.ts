/**
 * `turn.live` projection (live-view design §2.2, §3.3 batch C).
 *
 * Pure: observations in, one latched phase out. The Rust live layer
 * (`remuda-signal::live`) rides the nine phase spellings as tags in the raw
 * turn lifecycle's `relatedIds`; nothing is transported per spinner frame.
 * This module never invents a phase: an event without a `phase` tag leaves the
 * latch untouched ("unknown never collapses to idle" — design §2.2 rule 3),
 * and the OSC/screen tiers, which carry only a busy bit, never appear here as
 * content (design §2.4).
 */
import type { Observation } from "../../../types/generated";
import { nativeLifecycle } from "./payloadGuard";

/** The nine phases, verbatim from design §2.2 / harness-parity §6.2. */
export const LIVE_PHASES = [
  "prompt-accepted",
  "thinking",
  "tool-started",
  "tool-output",
  "tool-finished",
  "text-streaming",
  "turn-ended",
  "blocked",
  "interrupted",
] as const;

export type LivePhaseName = (typeof LIVE_PHASES)[number];

const PHASE_SET: ReadonlySet<string> = new Set(LIVE_PHASES);

/** Phases while the turn is live; the elapsed reading keeps ticking. */
const ACTIVE_PHASES: ReadonlySet<LivePhaseName> = new Set([
  "prompt-accepted",
  "thinking",
  "tool-started",
  "tool-output",
  "text-streaming",
  "blocked",
]);

/** Key names in `relatedIds`, mirrored from `remuda-signal::live`. */
const TAGS = {
  phase: "phase",
  since: "since",
  provision: "provision",
  tier: "tier",
  toolCallId: "toolCallId",
  toolName: "toolName",
  messageId: "messageId",
  phrase: "phrase",
  outcome: "outcome",
  promptId: "promptId",
} as const;

/** The latched phase the strip renders. All tag fields are read-only evidence. */
export type LivePhase = {
  phase: LivePhaseName;
  /** RFC3339-ms anchor of this transition; the browser renders `now − since`. */
  since: string | null;
  /** observation timestamp the transition was last seen at. */
  observedAt: string | null;
  /** SignalTier spelling that produced this phase (`hook` today). */
  tier: string | null;
  /** CapabilityProvision spelling (`native`/`emulated`/`unknown`). */
  provision: string | null;
  toolCallId: string | null;
  toolName: string | null;
  /** The once-per-transition spinner phrase, tier `screen`. Decoration only. */
  phrase: string | null;
  /** `completed` | `cancelled` | `failed` on `turn-ended`. */
  outcome: string | null;
  promptId: string | null;
};

export function isActivePhase(phase: LivePhaseName): boolean {
  return ACTIVE_PHASES.has(phase);
}

type TurnLifecycle = Extract<Observation, { kind: "lifecycle" }>;

function phaseTags(ev: TurnLifecycle): Record<string, string> | null {
  const payload = nativeLifecycle(ev);
  if (!payload) return null;
  // The phase tag rides the RAW hook lifecycle (batch A — one observation per
  // hook event, not a synthesized `turn.live`), so its `topic` is whatever
  // the raw event carried: `turn` for UserPromptSubmit/Stop, `diagnostic` for
  // Pre/PostToolUse, `hook` for MessageDisplay, `permission` for a blocking
  // request. Identity is therefore the `phase` tag alone, on a *native*
  // lifecycle; entity lifecycles (the OSC/screen busy bit) never carry it.
  const tags = (payload.relatedIds ?? {}) as Record<string, string>;
  const phase = tags[TAGS.phase];
  if (!phase || !PHASE_SET.has(phase)) return null;
  // c-cardsettle r3 item 8 / r4 item 3: a phase tag on a SUBAGENT-scoped
  // observation (non-empty agentId; agentType is optional) ends the
  // subagent's turn, never the root turn. Old journals can still carry this
  // tag from before the Node scoped it, so the browser must ignore it too —
  // otherwise the main strip shows 回合结束·失败 while the process is still
  // serving the workflow. Root turn boundaries carry no agentId.
  if (tags.agentId) return null;
  return tags;
}

/**
 * Identity of one latch episode. A repeated hook is not a new transition:
 * tags for the same episode keep the *earliest* `since`, so a re-fired
 * `PreToolUse` or a second `MessageDisplay` chunk cannot move the anchor.
 * Tool episodes are per tool call, text episodes per message; the other
 * phases latch by name alone.
 */
function episodeKey(phase: LivePhaseName, tags: Record<string, string>): string {
  if (phase === "tool-started" || phase === "tool-finished" || phase === "tool-output") {
    return `${phase}:${tags[TAGS.toolCallId] ?? ""}`;
  }
  if (phase === "text-streaming") return `${phase}:${tags[TAGS.messageId] ?? ""}`;
  return phase;
}

function seqValue(ev: Observation): bigint {
  try {
    return BigInt(ev.seq);
  } catch {
    return 0n;
  }
}

function compareSeq(a: Observation, b: Observation): number {
  const left = seqValue(a);
  const right = seqValue(b);
  return left < right ? -1 : left > right ? 1 : 0;
}

/**
 * Fold observations to the one current `turn.live` phase.
 *
 * Gap backfill can append older-seq events to the end of the store array, so
 * the input is ordered by `seq`, never encounter order. An event that carries
 * no phase tag (legacy lifecycle, OSC busy-bit, entity lifecycle) is invisible
 * here by construction.
 */
export function livePhase(events: readonly Observation[]): LivePhase | null {
  const tagged: TurnLifecycle[] = [];
  for (const ev of events) {
    if (ev.kind !== "lifecycle") continue;
    if (phaseTags(ev)) tagged.push(ev);
  }
  tagged.sort((a, b) => {
    const left = seqValue(a);
    const right = seqValue(b);
    return left < right ? -1 : left > right ? 1 : 0;
  });

  // Earliest anchor seen per episode, so re-deliveries never restart a clock.
  const firstSince = new Map<string, string | null>();
  let current: LivePhase | null = null;
  let currentKey = "";

  for (const ev of tagged) {
    const tags = phaseTags(ev)!;
    const phase = tags[TAGS.phase] as LivePhaseName;
    const key = episodeKey(phase, tags);
    const taggedSince = tags[TAGS.since] ?? null;
    if (!firstSince.has(key)) firstSince.set(key, taggedSince);
    else if (firstSince.get(key) === null && taggedSince) firstSince.set(key, taggedSince);
    if (key === currentKey) {
      // Same episode, later evidence chunk: keep the latched anchor. The
      // phrase may rotate mid-turn (design §2.4); it is decoration, refresh it.
      if (current && tags[TAGS.phrase]) current.phrase = tags[TAGS.phrase];
      continue;
    }
    currentKey = key;
    current = {
      phase,
      since: firstSince.get(key) ?? taggedSince,
      observedAt: ev.observedAt,
      tier: tags[TAGS.tier] ?? null,
      provision: tags[TAGS.provision] ?? null,
      toolCallId: tags[TAGS.toolCallId] ?? null,
      toolName: tags[TAGS.toolName] ?? null,
      phrase: tags[TAGS.phrase] ?? null,
      outcome: tags[TAGS.outcome] ?? null,
      promptId: tags[TAGS.promptId] ?? null,
    };
  }
  return current;
}

/**
 * Anchor of every running tool, keyed by a *content fingerprint*
 * (`name` + canonical input JSON), not by node id.
 *
 * The hook tier and the promoted-terminal transcript tailer do not yet share
 * node ids on the wire (the driver's transcript mapper still mints random
 * ids), so the projection converges them by content; the fingerprint is the
 * join key both sides compute independently from identical tool_name/input.
 * The bus emits the `ToolCall` extra as the very next observation after the
 * tagged lifecycle (one fold), so the start pairs with its hook payload —
 * which carries the input the fingerprint needs — by seq adjacency.
 */
export type ToolAnchor = { since: string; tier: string };

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${stableJson(record[key])}`)
    .join(",")}}`;
}

/** Content identity shared by the hook payload and the transcript block. */
export function toolFingerprint(name: string | null | undefined, input: unknown): string {
  return `${name ?? ""} ${stableJson(input)}`;
}

export function toolAnchors(events: readonly Observation[]): Map<string, ToolAnchor> {
  const starts = events
    .filter((ev): ev is TurnLifecycle => ev.kind === "lifecycle")
    .filter((ev) => phaseTags(ev)?.[TAGS.phase] === "tool-started")
    .slice()
    .sort(compareSeq);
  const calls = events
    .filter((ev): ev is Extract<Observation, { kind: "tool_call" }> => ev.kind === "tool_call")
    .slice()
    .sort(compareSeq);

  const anchors = new Map<string, ToolAnchor>();
  const knownNative = new Set<string>();
  // Both lists are seq-sorted and the hook extra trails the tagged lifecycle,
  // so a monotonic pointer pairs each start with its running ToolCall.
  let cursor = 0;
  for (const start of starts) {
    const tags = phaseTags(start)!;
    const nativeId = tags[TAGS.toolCallId];
    const startSeq = seqValue(start);
    while (cursor < calls.length && seqValue(calls[cursor]!) <= startSeq) cursor += 1;
    // A re-fired hook describes the same tool start; it walks the cursor but
    // never claims a new anchor.
    if (nativeId && knownNative.has(nativeId)) continue;
    if (!nativeId || !tags[TAGS.since] || cursor >= calls.length) continue;
    const call = calls[cursor]!.payload as {
      toolName?: { state?: string; value?: string } | string;
      input?: { state?: string; value?: unknown } | unknown;
    };
    cursor += 1;
    knownNative.add(nativeId);
    const knownValue = (value: unknown): unknown => {
      if (value && typeof value === "object" && (value as { state?: string }).state === "known") {
        return (value as { value: unknown }).value;
      }
      return value;
    };
    const name = knownValue(call.toolName) as string | undefined;
    anchors.set(toolFingerprint(name, knownValue(call.input)), {
      since: tags[TAGS.since]!,
      tier: tags[TAGS.tier] ?? "hook",
    });
  }
  return anchors;
}
