import type { Observation } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";

export const LONG_EVENT_COUNT = 2000;
export const LONG_SESSION_TITLE = "2000-event fixture";

export function buildLongObservations(opts: {
  instanceId: Id;
  journalId: Id;
  hostId: Id;
  count?: number;
}): Observation[] {
  const count = opts.count ?? LONG_EVENT_COUNT;
  const ts = "2026-09-12T00:00:00.000Z";
  const source = {
    driverKind: "claude-print",
    driverVersion: "2.1.268",
    adapterVersion: "0.1.0",
    channel: "stdout" as const,
    delivery: "replay" as const,
    nativeSessionId: unknownKnowledge("none"),
    nativeTurnId: unknownKnowledge("none"),
    nativeAgentId: unknownKnowledge("none"),
    nativeItemId: unknownKnowledge("none"),
    nativeEventId: unknownKnowledge("none"),
    nativeRequestId: { type: "none" as const },
    sourceCursor: { type: "runtime" as const, ledgerRevision: "1" },
  };
  const events: Observation[] = [];
  for (let i = 1; i <= count; i++) {
    const user = i % 2 === 1;
    events.push({
      schemaVersion: 1,
      eventId: `evt_long_${i}` as Id,
      journalId: opts.journalId,
      instanceId: opts.instanceId,
      runId: null,
      hostId: opts.hostId,
      processGeneration: "1",
      runGeneration: null,
      seq: String(i),
      observedAt: ts,
      nativeAt: known(ts),
      source,
      kind: "message",
      completeness: "structured",
      rawRef: null,
      evidenceEventIds: [],
      payload: {
        nodeId: `obj_long_n_${i}` as Id,
        revision: "1",
        operation: "open",
        baseRevision: null,
        messageId: `obj_long_m_${i}` as Id,
        role: user ? "user" : "assistant",
        phase: user ? "input" : "final",
        blocks: [{ type: "text", text: user ? `prompt ${i}` : `reply ${i}` }],
        targetBlock: null,
        parentToolCallId: null,
        nativeOrigin: known(user ? "ui" : "assistant"),
        status: "complete",
      },
    } as Observation);
  }
  return events;
}
