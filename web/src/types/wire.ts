/** Wire primitives from docs/design/protocol.md §1.1. Field names copied. */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type U64 = string;
export type Timestamp = string;
export type Id = string;
export type Digest = string;

export type Knowledge<T> =
  | { state: "known"; value: T }
  | { state: "unknown"; reason: string; evidenceEventIds: Id[] }
  | { state: "not-applicable" };

export type EntityMeta = {
  id: Id;
  revision: U64;
  createdAt: Timestamp;
  updatedAt: Timestamp;
};

export type ActorRef = {
  principalId: Id;
  type: "human" | "bot" | "agent" | "system";
  deviceId: Id | null;
  instanceId: Id | null;
};

export function known<T>(value: T): Knowledge<T> {
  return { state: "known", value };
}

export function unknownKnowledge<T>(reason: string): Knowledge<T> {
  return { state: "unknown", reason, evidenceEventIds: [] };
}

export function na<T>(): Knowledge<T> {
  return { state: "not-applicable" };
}
