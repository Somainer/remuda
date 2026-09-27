/**
 * Defensive payload guards for the live folds.
 *
 * Journal rows are only validated at the Hub store boundary; the web's
 * `coerceObservation` passes `payload` through untouched (`as unknown as
 * Observation`), so a lifecycle event's payload is NOT guaranteed to match
 * the generated `LifecyclePayload` union. The Hub-authored
 * `node_epoch_changed` diagnostic, for example, is a native lifecycle with
 * no `status` field, and older/other rows can be partial or non-object.
 *
 * Every live projection therefore narrows through these guards instead of
 * reading `ev.payload.type` directly: a malformed record is skipped, and the
 * strip must never throw on any journal payload (UO-6b round 2).
 */
import type { Observation } from "../../../types/generated";

type LifecycleObservation = Extract<Observation, { kind: "lifecycle" }>;
type NativeLifecyclePayload = Extract<LifecycleObservation["payload"], { type: "native" }>;
type EntityLifecyclePayload = Extract<LifecycleObservation["payload"], { type: "entity" }>;

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/** The payload when this observation is a well-formed native lifecycle. */
export function nativeLifecycle(ev: Observation): NativeLifecyclePayload | null {
  if (ev.kind !== "lifecycle") return null;
  const rec = asRecord(ev.payload);
  return rec && rec.type === "native" ? (rec as unknown as NativeLifecyclePayload) : null;
}

/** The payload when this observation is a well-formed entity lifecycle. */
export function entityLifecycle(ev: Observation): EntityLifecyclePayload | null {
  if (ev.kind !== "lifecycle") return null;
  const rec = asRecord(ev.payload);
  return rec && rec.type === "entity" ? (rec as unknown as EntityLifecyclePayload) : null;
}

/** A usage observation's payload as an untrusted record, or null. */
export function usagePayload(ev: Observation): Record<string, unknown> | null {
  if (ev.kind !== "usage") return null;
  return asRecord(ev.payload);
}

/** A message observation's payload as an untrusted record, or null. */
export function messagePayload(ev: Observation): Record<string, unknown> | null {
  if (ev.kind !== "message") return null;
  return asRecord(ev.payload);
}
