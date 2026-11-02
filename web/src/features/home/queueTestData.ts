import type { Host, Instance } from "../../types/instance";
import type { Interaction, InteractionRequest, QuestionField } from "../../types/interaction";
import type { Task } from "../../types/generated";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import type { GlanceSource } from "./glance";

/**
 * Test-only builders for the ④ queue's unit tests (homeQueue.test.ts and
 * HomeQueue.test.tsx). Never imported by app code, so the bundle drops it.
 */

export const T0 = "2026-10-31T08:00:00.000Z";
export const T1 = "2026-10-31T09:00:00.000Z";
export const T2 = "2026-10-31T10:00:00.000Z";
export const NOW = Date.parse(T2);

export function inst(overrides: Partial<Instance> & { id: Id }): Instance {
  return {
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    driver: "claude-cli",
    lifecycle: "running",
    activity: known("idle"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {} as Instance["nativeRef"],
    processRef: {} as Instance["processRef"],
    specRevision: "1",
    launchId: unknownKnowledge("none"),
    capabilities: {} as Instance["capabilities"],
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "jrn_1",
    durableSeq: "9",
    exit: na(),
    ...overrides,
  } as Instance;
}

export function host(overrides: Partial<Host> & { id: Id }): Host {
  return {
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    label: "forge",
    ownerPrincipalId: "prin_1",
    state: "online",
    transport: { mode: "outbound-wss", endpointRef: "ep_1" },
    ...overrides,
  } as Host;
}

export const APPROVAL: Extract<InteractionRequest, { kind: "approval" }> = {
  kind: "approval",
  title: "Bash",
  description: "rm -rf target/debug",
  toolCallId: null,
  actionRef: "act_1",
  options: [
    { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: "act_1" },
    { id: "allow-always-0", label: "始终允许 (acceptEdits)", effect: "allow-session", nativeValueRef: "act_1" },
    { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "act_1" },
  ],
  requestedPermissionsRef: null,
  inputDigest: "sha256:abab",
};

export function field(overrides: Partial<QuestionField> = {}): QuestionField {
  return {
    id: "q0",
    title: "旧迁移 0042 删掉还是保留？",
    description: "迁移",
    input: "single-select",
    required: true,
    options: [
      { id: "删除", label: "删除" },
      { id: "保留", label: "保留" },
      { id: "改名", label: "改名 (Recommended)" },
    ],
    allowFreeText: true,
    sensitive: false,
    ...overrides,
  };
}

export function question(fields: QuestionField[] = [field()]): InteractionRequest {
  return { kind: "question", title: "AskUserQuestion", fields };
}

export const PLAN: InteractionRequest = {
  kind: "plan-review",
  title: "Plan: split the gate",
  planRef: "pln_1",
  planRevision: "1",
  planDigest: "sha256:cdcd",
  options: [
    { id: "approve", label: "批准", effect: "allow-once", nativeValueRef: "pln_1" },
    { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "pln_1" },
  ],
  allowFeedback: true,
  plan: "1. read the gate\n\n2. split it\n3. land it",
};

export function interaction(
  overrides: Partial<Interaction> & { id: Id; instanceId: Id },
): Interaction {
  const request = overrides.request ?? APPROVAL;
  return {
    revision: "1",
    createdAt: T1,
    updatedAt: T1,
    runId: null,
    hostId: "hst_1",
    kind: request.kind,
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "tool" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "hst_1",
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request,
    deadline: unknownKnowledge("none"),
    deadlineSource: "none",
    answer: na(),
    delivery: "not-sent",
    resolution: na(),
    ...overrides,
  } as Interaction;
}

export function task(overrides: Partial<Task> & { id: string }): Task {
  return {
    projectId: "prj_1",
    title: "task",
    class: "implement",
    state: "running",
    budget: {},
    mandate: {} as Task["mandate"],
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    blockedReason: null,
    ...overrides,
  } as Task;
}

export function source(overrides: Partial<GlanceSource> = {}): GlanceSource {
  return {
    instances: [],
    interactions: [],
    hosts: [host({ id: "hst_1" })],
    tasks: [],
    answering: {},
    deviceId: "dev_1",
    nowMs: NOW,
    ...overrides,
  };
}
