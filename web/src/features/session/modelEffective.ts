/**
 * §9.1 effective-model view layer (sibling of `effortEffective.ts`).
 *
 * Read-back side: the resolved model id from the `/model` stdout verdict or
 * `message.model`, plus the discovered catalog the picker renders. The UI
 * always renders/selects from the *observed* id — a typed alias can resolve to
 * a different concrete gateway id (e.g. `sonnet` → `model_hub/es1_orange_o48`).
 */

/** Sources the protocol attributes an effective model to. */
export type ModelEffectiveSource = "launch" | "slash" | "remuda" | "unknown";

/** Whether a Remuda switch picked an id the session's own list offered, or
 *  typed it verbatim and let the verdict decide. */
export type ModelSelectionPath = "listed" | "typed";

/** Effective model observation view. */
export type ModelEffectiveView = {
  id: string;
  source: ModelEffectiveSource;
  observedAt: string;
  /** Present on a Remuda switch: listed = the id was in the session's own
   *  resolved catalog; typed = a verbatim `/model <id>` fallback. */
  selectionPath?: ModelSelectionPath;
};

/** Which discovery cache file answered. */
export type ModelCacheScope = "scoped-config-dir" | "host-fallback";

/** Gateway cache provenance recorded with a catalog resolution. */
export type ModelCacheView = {
  scope: ModelCacheScope;
  baseUrl?: string | null;
  fetchedAt?: string | null;
};

/** Where the model list came from. */
export type ModelCatalogSource = "gateway-discovery" | "settings" | "builtin";

/** The switchable model list for one session. */
export type ModelCatalogView = {
  models: string[];
  source: ModelCatalogSource;
  observedAt: string;
  /** Present for gateway-discovery answers: which cache file answered. */
  cache?: ModelCacheView | null;
  /** Whether CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY was in the launch
   *  environment, when the driver could tell. */
  discoveryEnv?: boolean | null;
};

/** Journal `model` observation payload (kept structural, not codegen-bound). */
type ModelObservationPayload = {
  kind: "model";
  payload: {
    requested?: string | null;
    effective: {
      id: string;
      source?: string;
      observedAt?: string;
    };
    catalog?: {
      models?: unknown;
      source?: string;
      observedAt?: string;
      cache?: unknown;
      discoveryEnv?: unknown;
    } | null;
    raw?: string | null;
    selectionPath?: unknown;
  };
};

const SOURCES: ReadonlySet<string> = new Set(["launch", "slash", "remuda", "unknown"]);
const CATALOG_SOURCES: ReadonlySet<string> = new Set([
  "gateway-discovery",
  "settings",
  "builtin",
]);

/** Normalize a `modelEffective` object off a Hub InstanceRecord. */
export function modelFromRecord(value: unknown): ModelEffectiveView | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id !== "string" || !record.id) return null;
  const source =
    typeof record.source === "string" && SOURCES.has(record.source)
      ? (record.source as ModelEffectiveSource)
      : "unknown";
  return {
    id: record.id,
    source,
    observedAt: typeof record.observedAt === "string" ? record.observedAt : "",
    selectionPath: selectionPathOf(record.selectionPath),
  };
}

function selectionPathOf(value: unknown): ModelSelectionPath | undefined {
  return value === "listed" || value === "typed" ? value : undefined;
}

function cacheFromRecord(value: unknown): ModelCacheView | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (record.scope !== "scoped-config-dir" && record.scope !== "host-fallback") return null;
  return {
    scope: record.scope,
    baseUrl: typeof record.baseUrl === "string" ? record.baseUrl : null,
    fetchedAt: typeof record.fetchedAt === "string" ? record.fetchedAt : null,
  };
}

/** Normalize a `modelCatalog` object off a Hub InstanceRecord. */
export function catalogFromRecord(value: unknown): ModelCatalogView | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (!Array.isArray(record.models)) return null;
  const models = record.models.filter((m): m is string => typeof m === "string" && !!m);
  if (!models.length) return null;
  const source =
    typeof record.source === "string" && CATALOG_SOURCES.has(record.source)
      ? (record.source as ModelCatalogSource)
      : "builtin";
  return {
    models,
    source,
    observedAt: typeof record.observedAt === "string" ? record.observedAt : "",
    cache: cacheFromRecord(record.cache),
    discoveryEnv: typeof record.discoveryEnv === "boolean" ? record.discoveryEnv : null,
  };
}

/** Extract effective model + optional catalog from a `model` observation. */
export function modelFromObservation(
  observation: unknown,
): {
  effective: ModelEffectiveView;
  catalog: ModelCatalogView | null;
  /** True when the payload explicitly carried a `requested` id (a launch
   *  snapshot or a switch verdict). A catalog refresh carries none. */
  hasRequested: boolean;
} | null {
  const event = observation as { body?: ModelObservationPayload } | null;
  const body = event?.body;
  // Accept both a nested body and a flattened shape defensively.
  const payload = body ?? (observation as ModelObservationPayload | null);
  if (!payload || payload.kind !== "model" || !payload.payload?.effective) return null;
  const effective = modelFromRecord(payload.payload.effective);
  if (!effective) return null;
  // Observation-level selection path (the driver stamps it outside
  // `effective`); honour it when the inner object did not carry one.
  const selectionPath =
    effective.selectionPath ?? selectionPathOf(payload.payload.selectionPath);
  return {
    effective: selectionPath ? { ...effective, selectionPath } : effective,
    catalog: catalogFromRecord(payload.payload.catalog),
    hasRequested: typeof payload.payload.requested === "string" && payload.payload.requested.length > 0,
  };
}

/** A recorded launch model-pin divergence (model-pin-1 §5): the Node's
 *  `model_pin_mismatch` warning diagnostic, verbatim. The client never
 *  recomputes the divergence — it reads this authoritative record. */
export type ModelPinMismatch = {
  requested: string;
  observed: string;
  /** When the diagnostic was recorded (Hub projection); absent for a record
   *  seen only in the live journal window. */
  observedAt?: string;
  /** Stable event id, used as the React key when more than one is recorded. */
  eventId?: string;
};

/** Extract every recorded `model_pin_mismatch` diagnostic from a journal
 *  event window, in journal order. A later `/model` does not erase history —
 *  the diagnostic stays in run details even though the chip moves on. */
export function modelPinMismatches(events: readonly unknown[]): ModelPinMismatch[] {
  const out: ModelPinMismatch[] = [];
  for (const event of events) {
    const e = event as
      | {
          eventId?: string;
          kind?: string;
          payload?: {
            type?: string;
            topic?: string;
            nativeName?: string;
            relatedIds?: Record<string, unknown>;
          };
        }
      | null;
    const p = e?.payload;
    if (
      e?.kind === "lifecycle" &&
      p?.type === "native" &&
      p.topic === "diagnostic" &&
      p.nativeName === "model_pin_mismatch"
    ) {
      const requested = p.relatedIds?.requested;
      const observed = p.relatedIds?.observed;
      if (typeof requested === "string" && typeof observed === "string") {
        out.push({ requested, observed, ...(e.eventId ? { eventId: e.eventId } : {}) });
      }
    }
  }
  return out;
}

/** Merge the Hub-projected launch divergences (durable, window-independent)
 *  with any diagnostics present in the currently loaded journal window
 *  (the live edge before projection lands), de-duplicated on
 *  requested/observed/observedAt. Projected records come first in stored
 *  order, then window-only records. */
export function allModelPinMismatches(
  projected:
    | readonly { requested?: unknown; observed?: unknown; observedAt?: unknown }[]
    | null
    | undefined,
  events: readonly unknown[],
): ModelPinMismatch[] {
  const out: ModelPinMismatch[] = [];
  const seen = new Set<string>();
  const keyOf = (r: { requested: string; observed: string; observedAt?: string }) =>
    `${r.requested}\u0000${r.observed}\u0000${r.observedAt ?? ""}`;
  const push = (r: ModelPinMismatch & { observedAt?: string }) => {
    const key = keyOf(r);
    if (seen.has(key)) return;
    seen.add(key);
    out.push(r);
  };
  for (const row of projected ?? []) {
    if (typeof row.requested === "string" && typeof row.observed === "string") {
      push({
        requested: row.requested,
        observed: row.observed,
        ...(typeof row.observedAt === "string" ? { observedAt: row.observedAt } : {}),
      });
    }
  }
  for (const mismatch of modelPinMismatches(events)) push(mismatch);
  return out;
}
