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
      name: string;
      ultracode?: boolean | null;
      source?: string;
      observedAt?: string;
    };
    raw?: string | null;
  };
};

const SOURCES: ReadonlySet<string> = new Set(["launch", "slash", "remuda", "unknown"]);

/** Normalize an `effortEffective` object off a Hub InstanceRecord. */
export function effectiveFromRecord(value: unknown): EffortEffectiveView | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
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

/** Extract effective effort from an observation, when it is an `effort` event. */
export function effectiveFromObservation(
  observation: unknown,
): { effective: EffortEffectiveView; requested?: { name?: string; ultracode?: boolean } } | null {
  const event = observation as { body?: EffortObservationPayload } | null;
  const body = event?.body;
  // Observations are `{kind, payload}` tagged enums; accept both a nested body
  // and a flattened shape defensively.
  const payload = body ?? (observation as EffortObservationPayload | null);
  if (!payload || payload.kind !== "effort" || !payload.payload?.effective) return null;
  const effective = payload.payload.effective;
  const view = effectiveFromRecord(effective);
  if (!view) return null;
  const requested = payload.payload.requested ?? undefined;
  return requested ? { effective: view, requested } : { effective: view };
}

/** Display name for an effective level: `ultracode` reads back as xhigh, and
 * when the flag was positively observed we keep the xhigh word (the flag is
 * shown by the ember of the request side). */
export function effectiveLabel(effective: EffortEffectiveView | null | undefined): string {
  return effective ? effective.name : "?";
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
