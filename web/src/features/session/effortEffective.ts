/**
 * §9.1 effective-effort view layer.
 *
 * D-056 splits the comparison into TWO axes: the level (`low…max`) and the
 * orthogonal ultracode switch. Each axis shows its own effective state and
 * its own 请求 → 实际 mismatch; the switch being unobserved is `?`, never a
 * guessed value.
 */

/** Sources the protocol attributes an effort edge to. */
export type EffortEffectiveSource = "launch" | "slash" | "remuda" | "unknown";

/** Wire shape of an effort-effective projection on the Hub instance record. */
export type EffortEffectiveView = {
  name: string;
  /**
   * Positively observed switch state. `true` after `Ultracode on`, `false`
   * after a positive off verdict/attachment, `null` while THIS process has
   * produced no switch evidence (the flag is read only from verdicts and
   * `ultra_effort_enter|exit` attachments, never inferred from an assistant
   * record — D-056 §4).
   */
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

const SOURCES = new Set(["launch", "slash", "remuda", "unknown"]);

/** Normalize an `effortEffective` object off a Hub-record/frame into a view. */
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

/** Extract effective effort from an observation. Accepts the wire envelope
 *  ({payload:{kind,payload:{effective}}}), a bare body
 *  ({kind:"effort",payload:{effective}}), and a straight body
 *  ({effective}). */
export function effectiveFromObservation(
  observation: unknown,
): { effective: EffortEffectiveView; requested?: { name?: string; ultracode?: boolean } } | null {
  if (!observation || typeof observation !== "object") return null;
  const record = observation as Record<string, unknown>;
  const payload =
    (record.payload as { payload?: { effective?: unknown }; effective?: unknown; kind?: string } | undefined) ?? null;
  const candidates: unknown[] = [];
  if (record.kind === "effort") candidates.push(record);
  if (payload && (payload.kind === "effort" || "effective" in payload)) candidates.push(payload);
  if (payload && typeof payload.payload === "object") candidates.push(payload.payload);
  for (const candidate of candidates) {
    if (!candidate || typeof candidate !== "object") continue;
    const body = candidate as { effective?: unknown; requested?: unknown };
    const effective = effectiveFromRecord(body.effective);
    if (effective) {
      const requested =
        body.requested && typeof body.requested === "object"
          ? (body.requested as { name?: string; ultracode?: boolean })
          : undefined;
      return { effective, requested };
    }
  }
  return null;
}

/** Whether no assistant record has ever reported a level for this session. */
export function isEffortUnknown(effective: EffortEffectiveView | null | undefined): boolean {
  return !effective;
}

/** Display name of the observed LEVEL (the switch renders separately). */
export function effectiveLabel(effective: EffortEffectiveView | null | undefined): string {
  if (!effective) return "?";
  return effective.name;
}

/** Display word of the observed SWITCH axis: on / off / unknown. */
export function effectiveUltraWord(effective: EffortEffectiveView | null | undefined): "on" | "off" | "?" {
  if (effective?.ultracode === true) return "on";
  if (effective?.ultracode === false) return "off";
  return "?";
}

/** Level-axis mismatch: the observed tier differs from the requested one. */
export function effortLevelMismatch(
  requestedName: string | null | undefined,
  effective: EffortEffectiveView | null | undefined,
): { requested: string; effective: string } | null {
  if (!effective || !requestedName) return null;
  return requestedName === effective.name
    ? null
    : { requested: requestedName, effective: effective.name };
}

/** How the observed switch differs from what was requested; null when it agrees. */
export type EffortFlagMismatch =
  /** Requested ON but the process positively reports off. */
  | { requested: "on"; observed: "off" }
  /** Requested ON and no process-local switch evidence yet. */
  | { requested: "on"; observed: "unknown" }
  /** Requested OFF while the process reports on. */
  | { requested: "off"; observed: "on" };

export function effortFlagMismatch(
  requestedUltracode: boolean,
  effective: EffortEffectiveView | null | undefined,
): EffortFlagMismatch | null {
  if (!effective) return null;
  if (requestedUltracode) {
    if (effective.ultracode === true) return null;
    return { requested: "on", observed: effective.ultracode === false ? "off" : "unknown" };
  }
  return effective.ultracode === true ? { requested: "off", observed: "on" } : null;
}

/** Whether an observed flag value delivers a definitive outcome for the
 *  switch axis. Any POSITIVE value (on OR off) settles — an opposite value is
 *  the delivered refusal (the mismatch renders); an unreported flag (null)
 *  proves nothing and keeps the indicator pending. Stale-vs-current is
 *  separately guarded by the request threshold. */
export function effortFlagSettles(observed: boolean | null | undefined): boolean {
  return observed === true || observed === false;
}

/**
 * Both axes at once, for the popover's per-axis hint lines.
 * `requestedWord` is the requested native level; `requestedUltracode` is the
 * switch state. (Kept under the old name as the single call site helper.)
 */
export function effortMismatch(
  requestedWord: string | null | undefined,
  requestedUltracode: boolean,
  effective: EffortEffectiveView | null | undefined,
): {
  level: { requested: string; effective: string } | null;
  flag: EffortFlagMismatch | null;
} {
  return {
    level: effortLevelMismatch(requestedWord, effective),
    flag: effortFlagMismatch(requestedUltracode, effective),
  };
}
