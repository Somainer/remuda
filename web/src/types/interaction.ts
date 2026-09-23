import type { NativeRequestKey } from "./nativeRef";
import type { ActorRef, Digest, EntityMeta, Id, Knowledge, Timestamp, U64 } from "./wire";

export type DecisionOption = {
  id: string;
  label: string;
  effect: "allow-once" | "allow-session" | "deny" | "cancel" | "native-specific";
  nativeValueRef: Id;
};

export type QuestionField = {
  id: string;
  title: string;
  description: string | null;
  input: "text" | "single-select" | "multi-select";
  required: boolean;
  options: { id: string; label: string; description?: string | null }[];
  allowFreeText: boolean;
  sensitive: boolean;
};

export type InteractionRequest =
  | {
      kind: "approval";
      title: string;
      description: string;
      toolCallId: Id | null;
      actionRef: Id;
      options: DecisionOption[];
      requestedPermissionsRef: Id | null;
      inputDigest: Digest;
    }
  | { kind: "question"; title: string; fields: QuestionField[] }
  | {
      kind: "plan-review";
      title: string;
      planRef: Id;
      planRevision: U64;
      planDigest: Digest;
      options: DecisionOption[];
      allowFeedback: boolean;
      /** Inline plan text under review; null/absent when only a planRef exists. */
      plan?: string | null;
    }
  | {
      kind: "elicitation";
      title: string;
      mode: "form" | "url" | "native-extension";
      schemaRef: Id | null;
      schemaDialect: string | null;
      url: string | null;
      nativeExtension: string | null;
      allowedActions: ("accept" | "decline" | "cancel")[];
    };

export type InteractionAnswer =
  | { kind: "approval"; optionId: string; inputDigest: Digest }
  | { kind: "question"; answers: Record<string, { optionIds: string[]; text: string | null }> }
  | { kind: "plan-review"; optionId: string; planRevision: U64; planDigest: Digest; feedback: string | null }
  | { kind: "elicitation"; action: "accept" | "decline" | "cancel"; content: unknown };

export type Interaction = EntityMeta & {
  instanceId: Id;
  runId: Id | null;
  hostId: Id;
  kind: "approval" | "question" | "plan-review" | "elicitation";
  requestKey: {
    native: NativeRequestKey;
    processGeneration: U64;
    runGeneration: U64 | null;
    connectionEpoch: Id;
  };
  requestVersion: U64;
  state: "pending" | "answer-committed" | "resolved" | "expired" | "invalidated" | "unknown" | "reconciling";
  blocking: boolean;
  answerable: boolean;
  carrier:
    | "claude-control"
    | "claude-hook"
    | "harness-hook"
    | "codex-rpc"
    | "acp-rpc"
    | "native-tty"
    | "unsupported";
  request: InteractionRequest;
  deadline: Knowledge<Timestamp>;
  deadlineSource: "native" | "runtime-policy" | "none" | "unknown";
  answer: Knowledge<{
    commandId: Id;
    actor: ActorRef;
    value: InteractionAnswer;
    committedAt: Timestamp;
  }>;
  delivery: "not-sent" | "intent-durable" | "written" | "confirmed" | "rejected" | "unknown";
  resolution: Knowledge<{
    reason: "answered" | "native-cleared" | "native-cancelled" | "generation-ended" | "timed-out";
    eventIds: Id[];
  }>;
};
