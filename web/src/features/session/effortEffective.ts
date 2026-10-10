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
  /** The projected effective state, or null when still unknown/withdrawn. */
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
 *  Accepts the wire envelope (`{body:{payload:{kind,payload:{effective}}}}`),
 *  a bare body (`{kind:"effort",payload:{effective}}`), and a flattened body
 *  (`{effective}`). An effort event whose effective is the
 *  read-back-unavailable edge returns `{effective:null, withdrawn:true}`. */
export function effectiveFromObservation(observation: unknown): EffortObservationResult | null {
  if (!observation || typeof observation !== "object") return null;
  const record = observation as Record<string, unknown>;
  const payload =
    (record.payload as
      | { payload?: { effective?: unknown; requested?: unknown }; effective?: unknown; kind?: string }
      | undefined) ?? null;
  const candidates: unknown[] = [];
  if (record.kind === "effort") candidates.push(record);
  if (payload && (payload.kind === "effort" || "effective" in payload)) candidates.push(payload);
  if (payload && typeof payload.payload === "object") candidates.push(payload.payload);
  for (const candidate of candidates) {
    if (!candidate || typeof candidate !== "object") continue;
    const body = candidate as {
      effective?: unknown;
      requested?: unknown;
    };
    const effectiveRecord =
      body.effective && typeof body.effective === "object"
        ? (body.effective as Record<string, unknown>)
        : null;
    if (!effectiveRecord) continue;
    const withdrawn = readbackWithdrawn(effectiveRecord);
    const view = effectiveFromRecord(effectiveRecord);
    const requested =
      body.requested && typeof body.requested === "object"
        ? (body.requested as { name?: string; ultracode?: boolean })
        : undefined;
    if (withdrawn) {
      const observedAt =
        typeof effectiveRecord.observedAt === "string" ? effectiveRecord.observedAt : undefined;
      return {
        effective: null,
        withdrawn: true,
        ...(observedAt ? { observedAt } : {}),
        ...(requested ? { requested } : {}),
      };
    }
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
