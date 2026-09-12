import type { ActorRef, Digest, EntityMeta, Id, Knowledge, U64 } from "./wire";

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
  expected?: { instanceRevision?: U64; processGeneration?: U64 };
};

export type CommandResult = { command: Command; relatedCommandIds: Id[] };

export type Page<T> = { items: T[]; nextCursor: string | null };

export function knowledgeValue<T>(k: Knowledge<T>): T | undefined {
  return k.state === "known" ? k.value : undefined;
}
