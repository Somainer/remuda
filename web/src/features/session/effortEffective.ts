/**
 * §9.1 effective-effort view layer.
 *
 * The slider/request side lives in `effort.ts` (owned by the slider workstream).
 * This module is the **read-back** side: what the native transcript actually
 * reports. The UI always renders the effective level; when nothing has been
 * observed yet it shows a greyed `?` and must never substitute the requested
 * level — requested and effective differing is the visible exit for an org
 * cap, a clamp, or an ultracode downgrade.
 */

/** Sources the protocol attributes an effective level to. */
export type EffortEffectiveSource = "launch" | "slash" | "remuda" | "unknown";

/** Wire shape of an `effort` observation's `effective` payload. */
export type EffortEffectiveView = {
  name: string;
  ultracode?: boolean | null;
  source: EffortEffectiveSource;
  observedAt: string;
};

/** Journal `effort` observation payload (kept structural, not codegen-bound). */
export type EffortObservationPayload = {
  kind: "effort";
  payload: {
    requested?: { name?: string; ultracode?: boolean } | null;
    effective: {
      name?: string | null;
      ultracode?: boolean | null;
      source?: string;
      observedAt?: string;
      /** D-056 (4): false withdraws the projected level/flag. */
      readbackAvailable?: boolean | null;
    } | null;
    raw?: string | null;
  };
};

/** Whether an effective record withdraws read-back (D-056 (4)). */
function readbackWithdrawn(record: Record<string, unknown>): boolean {
  return record.readbackAvailable === false;
}

const SOURCES: ReadonlySet<string> = new Set(["launch", "slash", "remuda", "unknown"]);

/** Normalize an `effortEffective` object off a Hub InstanceRecord.
 *  Returns null both before the first observation AND when the driver
 *  withdraws read-back (`readbackAvailable:false`, name/flag null) — the UI
 *  renders `?` and a pending switch is never treated as applied. */
export function effectiveFromRecord(value: unknown): EffortEffectiveView | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (readbackWithdrawn(record)) return null;
  if (typeof record.name !== "string" || !record.name) return null;
  const source =
    typeof record.source === "string" && SOURCES.has(record.source)
      ? (record.source as EffortEffectiveSource)
      : "unknown";
  return {
    name: record.name,
    ultracode: typeof record.ultracode === "boolean" ? record.ultracode : null,
    source,
    observedAt: typeof record.observedAt === "string" ? record.observedAt : "",
  };
}

/** Result of folding one `effort` observation. */
export type EffortObservationResult = {
  /** The projected effective state, or null when still unknown. */
  effective: EffortEffectiveView | null;
  /** True on the read-back-unavailable edge: the driver withdrew the previous
   *  projection; consumers must clear it WITHOUT settling or deleting a
   *  pending switch. */
  withdrawn: boolean;
  /** Timestamp carried by a withdrawn edge, for stale/ordering checks. */
  observedAt?: string;
  requested?: { name?: string; ultracode?: boolean };
};

/** Extract effective effort from an observation, when it is an `effort` event.
 *  Returns null for non-effort events. An effort event whose effective is the
 *  read-back-unavailable edge returns `{effective:null, withdrawn:true}`. */
export function effectiveFromObservation(observation: unknown): EffortObservationResult | null {
  const event = observation as { body?: EffortObservationPayload } | null;
  const body = event?.body;
  // Observations are `{kind, payload}` tagged enums; accept both a nested body
  // and a flattened shape defensively.
  const payload = body ?? (observation as EffortObservationPayload | null);
  if (!payload || payload.kind !== "effort" || !payload.payload?.effective) return null;
  const effectiveRecord = payload.payload.effective as Record<string, unknown>;
  const withdrawn = readbackWithdrawn(effectiveRecord);
  const view = effectiveFromRecord(effectiveRecord);
  const requested = payload.payload.requested ?? undefined;
  const withdrawnObservedAt =
    typeof effectiveRecord.observedAt === "string"
      ? (effectiveRecord.observedAt as string)
      : undefined;
  return {
    effective: view,
    withdrawn,
    ...(withdrawnObservedAt ? { observedAt: withdrawnObservedAt } : {}),
    ...(requested ? { requested } : {}),
  };
}

/** Display name for an effective level: a positively observed ultracode flag
 * reads back as tier xhigh but the chip shows the word the user actually
 * chose — `ultracode`. */
export function effectiveLabel(effective: EffortEffectiveView | null | undefined): string {
  if (!effective) return "?";
  return effective.ultracode === true ? "ultracode" : effective.name;
}

/** Whether no assistant record has ever reported a level for this session. */
export function isEffortUnknown(effective: EffortEffectiveView | null | undefined): boolean {
  return !effective;
}

/**
 * Requested vs effective mismatch text for the UI, or `null` when they agree
 * (or there is nothing observed yet — unknown is rendered separately as `?`,
 * never as a mismatch).
 *
 * `requestedWord` is the wire word the slider sent (`low…max | ultracode`);
 * `ultracode` reads back as level `xhigh`, which only counts as a mismatch
 * when the flag itself was not observed.
 */
export function effortMismatch(
  requestedWord: string | null | undefined,
  requestedUltracode: boolean,
  effective: EffortEffectiveView | null | undefined,
): { requested: string; effective: string } | null {
  if (!effective) return null;
  if (requestedUltracode) {
    // ultracode == xhigh + workflow. Observed xhigh with a positive flag is an
    // exact match; anything else (including a missing flag) is shown honestly
    // — we never assume the workflow is running.
    const matches = effective.name === "xhigh" && effective.ultracode === true;
    return matches ? null : { requested: "ultracode", effective: effective.name };
  }
  if (!requestedWord || requestedWord === effective.name) return null;
  return { requested: requestedWord, effective: effective.name };
}
