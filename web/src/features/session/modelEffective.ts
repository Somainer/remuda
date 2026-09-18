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

/** Effective model observation view. */
export type ModelEffectiveView = {
  id: string;
  source: ModelEffectiveSource;
  observedAt: string;
};

/** Where the model list came from. */
export type ModelCatalogSource = "gateway-discovery" | "settings" | "builtin";

/** The switchable model list for one session. */
export type ModelCatalogView = {
  models: string[];
  source: ModelCatalogSource;
  observedAt: string;
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
    } | null;
    raw?: string | null;
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
  };
}

/** Extract effective model + optional catalog from a `model` observation. */
export function modelFromObservation(
  observation: unknown,
): { effective: ModelEffectiveView; catalog: ModelCatalogView | null } | null {
  const event = observation as { body?: ModelObservationPayload } | null;
  const body = event?.body;
  // Accept both a nested body and a flattened shape defensively.
  const payload = body ?? (observation as ModelObservationPayload | null);
  if (!payload || payload.kind !== "model" || !payload.payload?.effective) return null;
  const effective = modelFromRecord(payload.payload.effective);
  if (!effective) return null;
  return { effective, catalog: catalogFromRecord(payload.payload.catalog) };
}
