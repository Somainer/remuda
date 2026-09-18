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

/** Outcome of comparing an observed model against the requested pin. */
export type ModelPinVerdict = "honoured" | "mismatch" | "unresolvable";

/** Drop a trailing context-window variant suffix (`…[1m]`) for comparison. */
function withoutContextSuffix(id: string): string {
  const trimmed = id.trim();
  const open = trimmed.lastIndexOf("[");
  return open >= 0 && trimmed.endsWith("]") ? trimmed.slice(0, open).trimEnd() : trimmed;
}

function isNamespaced(id: string): boolean {
  return id.includes("/");
}

/**
 * Compare an observed effective model against the requested pin.
 *
 * Mirrors `remuda_protocol::compare_model_pin` (model-pin-1 §3) — keep the two
 * in step. Two unequal ids are not necessarily a disagreement: a gateway
 * resolves a catalog id (`model_hub/es1_orange_o50[1m]`) to an upstream vendor
 * name (`claude-opus-5`), which is a correct launch, not a mismatch.
 *
 * - equal (or equal apart from a `[1m]` suffix) → `honoured`;
 * - both namespaced and different → `mismatch`;
 * - the pin is namespaced and the observation is an upstream name →
 *   `unresolvable` (report the id, never flag divergence);
 * - a catalog hit upgrades an un-namespaced observation to `mismatch`;
 * - two bare aliases that differ → `mismatch`.
 */
export function compareModelPin(
  requested: string,
  observed: string,
  catalog: readonly string[] = [],
): ModelPinVerdict {
  const pin = requested.trim();
  const seen = observed.trim();
  if (!pin || !seen) return "honoured";
  if (seen === pin || withoutContextSuffix(seen) === withoutContextSuffix(pin)) {
    return "honoured";
  }
  const inCatalog = (id: string): boolean =>
    catalog.some((entry) => entry === id || withoutContextSuffix(entry) === withoutContextSuffix(id));
  if (isNamespaced(pin) && isNamespaced(seen)) return "mismatch";
  if (isNamespaced(pin)) return inCatalog(seen) ? "mismatch" : "unresolvable";
  if (isNamespaced(seen)) return "unresolvable";
  return "mismatch";
}

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
