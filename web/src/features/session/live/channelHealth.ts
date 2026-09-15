/**
 * `ChannelHealth` — derived in the browser, never transported
 * (live-view design §2.6).
 *
 * Every input already exists on the client: the instance's expected signal
 * tiers and the journal event list. A tier that is expected but silent reads
 * `never-materialised` / `stalled`, so the D-4 failure (transcript persistence
 * silently disabled) shows as an explicit note instead of a quiet agent, and
 * the elapsed reading greys the moment the channel anchoring it goes quiet.
 */
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";

export type HealthReason = "ok" | "never-materialised" | "stalled" | "disabled";

export type TierHealth = {
  tier: string;
  expected: boolean;
  /** RFC3339 ms of the newest observation from this tier, this run. */
  lastRecordAt: string | null;
  reason: HealthReason;
};

/**
 * Expected quiet period per tier; `stalled` after 3× the cadence (design
 * §2.6). Hooks are event-driven: a long-running tool genuinely produces
 * nothing for tens of seconds, and the point is to say so rather than keep
 * claiming a fresh reading.
 */
export const TIER_CADENCE_MS: Record<string, number> = {
  hook: 2000,
  rpc: 1000,
  file: 5000,
  transcript: 5000,
  osc: 200,
  screen: 200,
  pty: 200,
};

const STALL_FACTOR = 3;

/** Wire `SourceChannel`s that count as evidence for one expected SignalTier. */
const CHANNELS_FOR_TIER: Record<string, readonly string[]> = {
  hook: ["hook"],
  rpc: ["rpc"],
  file: ["file", "transcript"],
  transcript: ["file", "transcript"],
  osc: ["osc", "pty"],
  screen: ["screen", "osc", "pty"],
};

/** Tiers the instance's native ref says this run can speak. */
export function expectedTiersFor(nativeRef: NativeRef | null | undefined): string[] {
  const tiers = new Set<string>();
  if (!nativeRef) return [];
  if (nativeRef.signalTier && nativeRef.signalTier !== "none") tiers.add(nativeRef.signalTier);
  for (const capability of nativeRef.capabilities ?? []) {
    if (capability.tier && capability.tier !== "none") tiers.add(capability.tier);
  }
  return [...tiers];
}

function evidenceChannels(tier: string): readonly string[] {
  return CHANNELS_FOR_TIER[tier] ?? [tier];
}

/**
 * Health of every *expected* tier as a pure function of the journal.
 * Disabled tiers are omitted entirely — the strip reports what the run was
 * supposed to have, never a complaint about an absent adapter.
 */
export function channelHealth(
  events: readonly Observation[],
  expectedTiers: readonly string[],
  nowMs: number = Date.now(),
): Map<string, TierHealth> {
  const lastByChannel = new Map<string, number>();
  for (const ev of events) {
    const channel = ev.source?.channel;
    if (!channel) continue;
    const at = Date.parse(ev.observedAt);
    if (Number.isNaN(at)) continue;
    const prev = lastByChannel.get(channel);
    if (prev === undefined || at > prev) lastByChannel.set(channel, at);
  }

  const out = new Map<string, TierHealth>();
  for (const tier of expectedTiers) {
    let lastAt: number | null = null;
    for (const channel of evidenceChannels(tier)) {
      const at = lastByChannel.get(channel);
      if (at !== undefined && (lastAt === null || at > lastAt)) lastAt = at;
    }
    let reason: HealthReason = "ok";
    if (lastAt === null) {
      reason = "never-materialised";
    } else {
      const cadence = TIER_CADENCE_MS[tier] ?? 2000;
      if (nowMs - lastAt > STALL_FACTOR * cadence) reason = "stalled";
    }
    out.set(tier, {
      tier,
      expected: true,
      lastRecordAt: lastAt === null ? null : new Date(lastAt).toISOString(),
      reason,
    });
  }
  return out;
}

/** True when the tier backing a reading cannot currently vouch for freshness. */
export function isUnfresh(health: TierHealth | undefined): boolean {
  return Boolean(health && health.reason !== "ok" && health.reason !== "disabled");
}
