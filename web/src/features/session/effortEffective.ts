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

/** Normalize an `effortEffective` object off a Hub-record/frame into a view.
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
  /** The projected effective state; null while unknown or withdrawn. */
  effective: EffortEffectiveView | null;
  /** True on the read-back-unavailable edge: the driver withdrew the previous
   *  projection; consumers must clear it WITHOUT settling or deleting a
   *  pending switch. */
  withdrawn: boolean;
  /** Timestamp carried by a withdrawn edge, for stale/ordering checks. */
  observedAt?: string;
  requested?: { name?: string; ultracode?: boolean };
};

/** Extract effective effort from an observation. Accepts the wire envelope
 *  ({body:{payload:{kind:"effort",payload:{effective}}}}), a bare body
 *  ({kind:"effort",payload:{effective}}), and a straight body ({effective}).
 *  Returns null for non-effort events. An effort event whose effective is the
 *  read-back-unavailable edge returns `{effective:null, withdrawn:true}`. */
export function effectiveFromObservation(observation: unknown): EffortObservationResult | null {
  if (!observation || typeof observation !== "object") return null;
  const record = observation as Record<string, unknown>;
  const body =
    record.body && typeof record.body === "object"
      ? (record.body as Record<string, unknown>)
      : null;
  const envelope =
    record.payload && typeof record.payload === "object"
      ? (record.payload as Record<string, unknown>)
      : null;
  const candidates: Record<string, unknown>[] = [];
  if (record.kind === "effort") candidates.push(record);
  if (body) candidates.push(body);
  if (envelope && (envelope.kind === "effort" || "effective" in envelope)) {
    candidates.push(envelope);
  }
  const nested = envelope?.payload;
  if (nested && typeof nested === "object") candidates.push(nested as Record<string, unknown>);
  for (const candidate of candidates) {
    if (!("effective" in candidate)) continue;
    const effectiveRecord = candidate.effective;
    if (!effectiveRecord || typeof effectiveRecord !== "object") continue;
    const eff = effectiveRecord as Record<string, unknown>;
    const requested =
      candidate.requested && typeof candidate.requested === "object"
        ? (candidate.requested as { name?: string; ultracode?: boolean })
        : undefined;
    if (readbackWithdrawn(eff)) {
      const observedAt = typeof eff.observedAt === "string" ? eff.observedAt : undefined;
      return {
        effective: null,
        withdrawn: true,
        ...(observedAt ? { observedAt } : {}),
        ...(requested ? { requested } : {}),
      };
    }
    const view = effectiveFromRecord(eff);
    if (view) {
      return { effective: view, withdrawn: false, ...(requested ? { requested } : {}) };
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
