/** Session/journal observation types. Overlaps re-export `generated.ts` (protocol wire). */
export type {
  Completeness,
  MessagePayload,
  Observation,
  ObservationKind,
  ObservationSource,
  OpaquePayload,
  ThoughtPayload,
  ToolCallPayload,
  ToolResultPayload,
  UsagePayload,
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
  WorkflowState,
} from "./generated";

import type { Observation } from "./generated";
import type { Interaction } from "./interaction";
import type { Id, U64 } from "./wire";

export type Snapshot = {
  projectionVersion: string;
  projectionEpoch: Id;
  asOfSeq: U64;
  instance: unknown;
  runs: unknown[];
  commands: unknown[];
  pendingInteractions: Interaction[];
  nodes: unknown[];
  history: { earliestRetainedSeq: U64; complete: boolean };
};

export type EventsBatch = {
  jsonrpc: "2.0";
  method: "events.batch";
  params: {
    subscriptionId: Id;
    journalId: Id;
    fromSeq: U64;
    toSeq: U64;
    events: Observation[];
    durableSeq: U64;
  };
};
