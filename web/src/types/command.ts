import type { ActorRef, Digest, EntityMeta, Id, Knowledge, U64 } from "./wire";

export type CommandSettlementOutcome = "cancelled" | "completed" | "expired" | "rejected";

export type Command = EntityMeta & {
  commandId: Id;
  actor: ActorRef;
  origin: "ui" | "bot" | "mcp" | "cli" | "system";
  operation: string;
  target: { hostId: Id; instanceId: Id | null; runId: Id | null };
  payloadDigest: Digest;
  state: "queued" | "accepted" | "settled";
  dispatch: "not-dispatched" | "intent-durable" | "transport-written" | "native-acknowledged";
  resolution: "clear" | "unknown" | "reconciling";
  /**
   * Present when state === "settled". `outcome: "rejected"` is a real Node
   * rejection (D-055: a rejected send must never be marked delivered and is
   * not retried). `reason` carries a human-readable cause.
   */
  settlement?: { outcome: CommandSettlementOutcome; reason?: string };
  expected?: { instanceRevision?: U64; processGeneration?: U64 };
};

export type CommandResult = { command: Command; relatedCommandIds: Id[] };

export type Page<T> = { items: T[]; nextCursor: string | null };

export function knowledgeValue<T>(k: Knowledge<T> | { state: string; value?: T }): T | undefined {
  return k.state === "known" ? (k.value as T) : undefined;
}
