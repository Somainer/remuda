import type { Observation } from "../types/observation";
import type { Id, U64 } from "../types/wire";
import { known, unknownKnowledge } from "../types/wire";

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

const STUB_SOURCE: Observation["source"] = {
  driverKind: "claude-print",
  driverVersion: "hub",
  adapterVersion: "0.1.0",
  channel: "stdout",
  delivery: "replay",
  nativeSessionId: unknownKnowledge("none"),
  nativeTurnId: unknownKnowledge("none"),
  nativeAgentId: unknownKnowledge("none"),
  nativeItemId: unknownKnowledge("none"),
  nativeEventId: unknownKnowledge("none"),
  nativeRequestId: { type: "none" },
  sourceCursor: { type: "runtime", ledgerRevision: "1" },
};

/** Hub journal rows wrap the observation in `{ event, seq, eventId }`. */
export function coerceObservation(raw: unknown, journalId: Id, instanceId: Id, fallbackSeq?: string): Observation | null {
  const row = asRecord(raw);
  if (!row) return null;
  const inner = asRecord(row.event) ?? row;
  const seq = String(inner.seq ?? row.seq ?? fallbackSeq ?? "0");
  const eventId = String(inner.eventId ?? row.eventId ?? `evt_${seq}`) as Id;
  const kind = (typeof inner.kind === "string" ? inner.kind : "opaque") as Observation["kind"];
  const payload = inner.payload ?? inner;
  return {
    schemaVersion: 1,
    eventId,
    journalId,
    instanceId,
    runId: null,
    hostId: (typeof inner.hostId === "string" ? inner.hostId : instanceId) as Id,
    processGeneration: "1",
    runGeneration: null,
    seq: seq as U64,
    observedAt: String(inner.observedAt ?? row.observedAt ?? new Date().toISOString()),
    nativeAt: known(String(inner.observedAt ?? row.observedAt ?? "")),
    source: STUB_SOURCE,
    kind,
    completeness: (typeof inner.completeness === "string" ? inner.completeness : "structured") as Observation["completeness"],
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as unknown as Observation;
}

export function coerceObservationList(raw: unknown, journalId: Id, instanceId: Id): Observation[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((row, i) => coerceObservation(row, journalId, instanceId, String(i + 1)))
    .filter((row): row is Observation => row != null);
}
