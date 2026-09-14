import type { Observation } from "../types/observation";
import type { Id, U64 } from "../types/wire";
import { known, unknownKnowledge } from "../types/wire";

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

function messagePayload(value: unknown, eventId: Id): Record<string, unknown> {
  const payload = asRecord(value) ?? {};
  const identity = (value: unknown) => typeof value === "string" && value.length > 0 ? value : null;
  const messageId = identity(payload.messageId) ?? identity(payload.nodeId) ?? eventId;
  const operation = payload.operation;
  // Older Hub journals (including the fake Node) store only { role, text }.
  // Give each such event its own identity while keeping protocol mutations intact.
  return {
    ...payload,
    messageId,
    nodeId: identity(payload.nodeId) ?? messageId,
    revision: typeof payload.revision === "string" && /^\d+$/.test(payload.revision) ? payload.revision : "1",
    baseRevision: payload.baseRevision ?? null,
    operation: operation === "append" || operation === "replace" || operation === "close" ? operation : "open",
    role: payload.role ?? "assistant",
    phase: payload.phase ?? (payload.role === "user" ? "input" : "final"),
    status: payload.status ?? "complete",
    blocks: Array.isArray(payload.blocks) ? payload.blocks : typeof payload.text === "string" ? [{ type: "text", text: payload.text }] : [],
    targetBlock: payload.targetBlock ?? null,
    parentToolCallId: payload.parentToolCallId ?? null,
    nativeOrigin: payload.nativeOrigin ?? unknownKnowledge("legacy journal message"),
    // `origin` is additive (protocol §5.2, D-028 P3). A journal that predates
    // it leaves it undefined, and the transcript reads that as the human's own
    // words rather than hiding the message.
    origin: payload.origin,
  };
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
    payload: kind === "message" ? messagePayload(payload, eventId) : payload,
  } as unknown as Observation;
}

export function coerceObservationList(raw: unknown, journalId: Id, instanceId: Id): Observation[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((row, i) => coerceObservation(row, journalId, instanceId, String(i + 1)))
    .filter((row): row is Observation => row != null);
}
