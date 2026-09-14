import claudeInit from "../fixtures/claude-p-init.json" with { type: "json" };
import type { Command, CommandResult, Page } from "../types/command";
import type { Host, Instance, Kind } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type {
  EventsBatch,
  Observation,
  ObservationSource,
  Snapshot,
  ToolCallPayload,
  ToolResultPayload,
} from "../types/observation";
import type { Workspace } from "../types/workspace";
import { known, unknownKnowledge, type Id, type U64 } from "../types/wire";
import { printCapabilities, ptyCapabilities } from "./capabilities";
import type { ResumeMode, ResumeResult } from "./api";
import type { DriverKind } from "../types/nativeRef";
import { HubHttpError } from "./httpError";
import { digestPlaceholder, id, now } from "./ids";
import { MOCK_BOOTSTRAP_TOKEN, readSession, type DeviceSession, type PairCode, type PairedDevice } from "./session";

export type MockPasskey = {
  id: string;
  name: string;
  createdAt: string;
  lastUsedAt: string | null;
  thisDevice: boolean;
};
import { thisDeviceId } from "./interactionStatus";
import { LONG_EVENT_COUNT, LONG_SESSION_TITLE, buildLongObservations } from "../fixtures/session/longEvents";

const ts = now();

function meta(entityId: Id) {
  return { id: entityId, revision: "1", createdAt: ts, updatedAt: ts };
}

function source(): ObservationSource {
  return {
    driverKind: "claude-print",
    driverVersion: "2.1.268",
    adapterVersion: "0.1.0",
    channel: "stdout",
    delivery: "replay",
    nativeSessionId: known(String(claudeInit.session_id)),
    nativeTurnId: unknownKnowledge("none"),
    nativeAgentId: unknownKnowledge("none"),
    nativeItemId: unknownKnowledge("none"),
    nativeEventId: unknownKnowledge("none"),
    nativeRequestId: { type: "none" },
    sourceCursor: { type: "runtime", ledgerRevision: "1" },
  };
}

const hostId = id("hst_");
const workspaceId = id("wsp_");
const journalWorking = id("obj_");
const journalBlocked = id("obj_");
const insWorking = id("ins_");
const insBlocked = id("ins_");
const insIdle = id("ins_");
const runWorking = id("run_");
const workflowId = id("obj_");
const phaseCompileId = id("obj_");
const interactionId = id("int_");
const nativeSession = String(claudeInit.session_id);

function instanceBase(entityId: Id, journal: Id, lifecycle: Instance["lifecycle"], activity: Instance["activity"]): Instance {
  const caps = printCapabilities();
  return {
    ...meta(entityId),
    hostId,
    workspaceId,
    kind: "claude",
    driver: "claude-print",
    lifecycle,
    activity,
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {
      hostId,
      nativeStoreId: id("obj_"),
      kind: "claude",
      sessionId: known(nativeSession),
      transcript: unknownKnowledge("not-exported"),
      claude: { sessionId: nativeSession },
    },
    processRef: {
      processGeneration: "1",
      processIdentity: unknownKnowledge("mock"),
      connectionEpoch: id("epoch_"),
    },
    specRevision: "1",
    launchId: known(id("launch_")),
    capabilities: caps,
    ownerFence: "1",
    activeRunIds: lifecycle === "ready" ? [runWorking] : [],
    parent: null,
    journalId: journal,
    durableSeq: "1",
    exit: { state: "not-applicable" },
  };
}

const hosts: Host[] = [
  {
    ...meta(hostId),
    label: "devbox-sg",
    ownerPrincipalId: id("prn_"),
    state: "online",
    transport: { mode: "outbound-wss", endpointRef: id("obj_") },
  },
];

const workspaces: Workspace[] = [
  {
    ...meta(workspaceId),
    hostId,
    label: "sfe-root",
    rootPath: "/home/devuser/Projects/sfe-root",
    writePolicy: "workspace-write",
    canonicalRoot: known("/home/devuser/Projects/sfe-root"),
  },
];

const instances: Instance[] = [
  instanceBase(insBlocked, journalBlocked, "ready", known("waiting-interaction")),
  instanceBase(insWorking, journalWorking, "ready", known("working")),
  { ...instanceBase(insIdle, id("obj_"), "ready", known("idle")), activeRunIds: [] },
];

const pendingInteraction: Interaction = {
  ...meta(interactionId),
  instanceId: insBlocked,
  runId: runWorking,
  hostId,
  kind: "approval",
  requestKey: {
    native: { type: "rpc", valueType: "string", value: "toolu_mock" },
    processGeneration: "1",
    runGeneration: "1",
    connectionEpoch: id("epoch_"),
  },
  requestVersion: "1",
  state: "pending",
  blocking: true,
  answerable: true,
  carrier: "claude-control",
  request: {
    kind: "approval",
    title: "Bash",
    description: "rm -rf /tmp/coord-media",
    toolCallId: id("obj_"),
    actionRef: id("obj_"),
    options: [
      { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: id("obj_") },
      { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: id("obj_") },
    ],
    requestedPermissionsRef: null,
    inputDigest: digestPlaceholder(),
  },
  deadline: unknownKnowledge("none"),
  deadlineSource: "none",
  answer: { state: "not-applicable" },
  delivery: "not-sent",
  resolution: { state: "not-applicable" },
};

function obs(
  instanceId: Id,
  journalId: Id,
  seq: number,
  kind: Observation["kind"],
  payload: unknown,
): Observation {
  return {
    schemaVersion: 1,
    eventId: id("evt_"),
    journalId,
    instanceId,
    runId: runWorking,
    hostId,
    processGeneration: "1",
    runGeneration: "1",
    seq: String(seq),
    observedAt: ts,
    nativeAt: known(ts),
    source: source(),
    kind,
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

function bashCall(): ToolCallPayload {
  return {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: id("obj_"),
    parentToolCallId: null,
    toolName: known("Bash"),
    displayTitle: known("Bash"),
    category: "shell",
    input: known({ command: "ninja -C build TaskManagerTest" }),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId, workspaceId, nativeAgentId: null }),
  };
}

function bashResult(toolCallId: Id): ToolResultPayload {
  return {
    nodeId: id("obj_"),
    revision: "1",
    operation: "close",
    baseRevision: null,
    toolCallId,
    stage: "final",
    outcome: "succeeded",
    blocks: [{ type: "text", text: "ninja: no work to do." }],
    structuredResult: unknownKnowledge("text"),
    exitCode: known(0),
    changes: [],
  };
}

const bash = bashCall();
const editCall: ToolCallPayload = {
  ...bashCall(),
  toolCallId: id("obj_"),
  toolName: known("Edit"),
  displayTitle: known("Edit"),
  category: "file-write",
  input: known({ file_path: "src/exec.cc", old_string: "spill", new_string: "spill_v2" }),
  state: "proposed",
};

const journals = new Map<Id, Observation[]>();

journals.set(journalWorking, [
  obs(insWorking, journalWorking, 1, "lifecycle", {
    type: "entity",
    entityType: "instance",
    entityId: insWorking,
    revision: "1",
    previousState: "starting",
    state: "ready",
    reasonCode: "init",
    evidenceEventIds: [],
    entity: { tools: claudeInit.tools, model: claudeInit.model, cwd: claudeInit.cwd, session_id: claudeInit.session_id },
  }),
  obs(insWorking, journalWorking, 2, "message", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    messageId: id("obj_"),
    role: "user",
    phase: "input",
    blocks: [{ type: "text", text: "看 TaskManager spill 这段为啥抖" }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("ui"),
    status: "complete",
  }),
  obs(insWorking, journalWorking, 3, "thought", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    thoughtId: id("obj_"),
    representation: "summary",
    text: "先读执行器再看 spill 路径。",
    partIndex: 0,
    status: "complete",
  }),
  obs(insWorking, journalWorking, 4, "tool_call", bash),
  obs(insWorking, journalWorking, 5, "tool_result", bashResult(bash.toolCallId)),
  obs(insWorking, journalWorking, 6, "tool_call", editCall),
  obs(insWorking, journalWorking, 7, "tool_result", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "close",
    baseRevision: null,
    toolCallId: editCall.toolCallId,
    stage: "final",
    outcome: "succeeded",
    blocks: [],
    structuredResult: unknownKnowledge("diff"),
    exitCode: { state: "not-applicable" },
    changes: [{ path: "src/exec.cc", diff: "@@\n-spill\n+spill_v2\n", application: "applied" }],
  }),
  obs(insWorking, journalWorking, 8, "tool_call", {
    ...bashCall(),
    toolCallId: id("obj_"),
    toolName: known("Read"),
    displayTitle: known("Read"),
    category: "file-read",
    input: known({ file_path: "src/exec.cc" }),
  }),
  obs(insWorking, journalWorking, 9, "tool_call", {
    ...bashCall(),
    toolCallId: id("obj_"),
    toolName: known("Write"),
    displayTitle: known("Write"),
    category: "file-write",
    input: known({ file_path: "notes.md", content: "# spill" }),
  }),
  obs(insWorking, journalWorking, 10, "tool_call", {
    ...bashCall(),
    toolCallId: id("obj_"),
    toolName: known("Workflow"),
    displayTitle: known("Workflow"),
    category: "workflow",
    input: known({ script: "agent({model:'passthrough/auto'})" }),
  }),
  obs(insWorking, journalWorking, 11, "workflow.run", {
    workflowId,
    engine: "claude-workflow",
    nativeRunId: known("wf_9f3"),
    nativeTaskId: unknownKnowledge("none"),
    toolCallId: null,
    state: "running",
    revision: "1",
    title: known("compile"),
    resultRef: null,
  }),
  obs(insWorking, journalWorking, 12, "workflow.phase", {
    workflowId,
    phaseId: phaseCompileId,
    nativePhaseId: known("compile"),
    label: known("compile"),
    state: "running",
    revision: "1",
    parentPhaseId: null,
  }),
  obs(insWorking, journalWorking, 13, "workflow.member", {
    workflowId,
    memberId: id("obj_"),
    nativeAgentId: known("agent-1"),
    nativeKey: known("haiku"),
    attempt: known("1"),
    phaseId: phaseCompileId,
    label: known("haiku"),
    state: "running",
    modelRequested: known("haiku"),
    modelResolved: known("claude-haiku-4-5-20251001"),
    resultRef: null,
    revision: "1",
    childInstanceId: insIdle,
  }),
  obs(insWorking, journalWorking, 14, "workflow.member", {
    workflowId,
    memberId: id("obj_"),
    nativeAgentId: known("agent-2"),
    nativeKey: known("sonnet"),
    attempt: known("1"),
    phaseId: null,
    label: known("sonnet-cold"),
    state: "completed",
    modelRequested: known("sonnet"),
    modelResolved: known("passthrough/auto"),
    resultRef: null,
    revision: "1",
  }),
  obs(insWorking, journalWorking, 15, "tool_call", {
    ...bashCall(),
    toolCallId: id("obj_"),
    toolName: known("Task"),
    displayTitle: known("Task"),
    category: "agent",
    input: known({ prompt: "summarize" }),
  }),
  obs(insWorking, journalWorking, 16, "tool_call", {
    ...bashCall(),
    toolCallId: id("obj_"),
    toolName: known("mcp__claude_ai_Google_Drive__search_files"),
    displayTitle: known("mcp search_files"),
    category: "mcp",
    input: known({ query: "spill" }),
  }),
  obs(insWorking, journalWorking, 17, "message", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    messageId: id("obj_"),
    role: "assistant",
    phase: "final",
    blocks: [{ type: "text", text: "Spill 抖动来自 TaskManager 在 `src/exec.cc` 的路径切换。已改一处。" }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("assistant"),
    status: "complete",
  }),
  obs(insWorking, journalWorking, 18, "usage", {
    usageId: id("obj_"),
    scope: "turn",
    scopeId: runWorking,
    mode: "snapshot",
    metricRevision: "1",
    inputTokens: known("12100"),
    inputAccounting: "unknown",
    outputTokens: known("800"),
    reasoningTokens: unknownKnowledge("none"),
    cacheReadTokens: unknownKnowledge("none"),
    cacheWriteTokens: unknownKnowledge("none"),
    totalTokens: unknownKnowledge("none"),
    cost: known({ amount: "0.12", currency: "USD" }),
    accounting: "estimated",
    nativeFieldsRef: null,
  }),
  obs(insWorking, journalWorking, 19, "opaque", {
    nativeType: "rate_limit_event",
    reason: "unmapped-fields",
    rawRef: {
      objectId: id("obj_"),
      offset: "0",
      length: "0",
      digest: digestPlaceholder(),
      mediaType: "application/json",
      redaction: "none",
    },
    affects: [],
    summary: "rate_limit_event",
  }),
]);

journals.set(journalBlocked, [
  obs(insBlocked, journalBlocked, 1, "message", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    messageId: id("obj_"),
    role: "user",
    phase: "input",
    blocks: [{ type: "text", text: "清一下 /tmp/coord-media" }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("ui"),
    status: "complete",
  }),
  obs(insBlocked, journalBlocked, 2, "interaction.requested", { interaction: pendingInteraction }),
]);

const interactions: Interaction[] = [pendingInteraction];
const questionId = id("int_");
const journalQuestion = id("obj_");
const insQuestion = id("ins_");
const questionInteraction: Interaction = {
  ...meta(questionId),
  instanceId: insQuestion,
  runId: runWorking,
  hostId,
  kind: "question",
  requestKey: {
    native: { type: "rpc", valueType: "string", value: "ask_user" },
    processGeneration: "1",
    runGeneration: "1",
    connectionEpoch: id("epoch_"),
  },
  requestVersion: "1",
  state: "pending",
  blocking: true,
  answerable: true,
  carrier: "claude-control",
  request: {
    kind: "question",
    title: "AskUserQuestion",
    fields: [
      {
        id: "path",
        title: "从哪条 spill 路径下手？",
        description: null,
        input: "single-select",
        required: true,
        options: [
          { id: "exec", label: "src/exec.cc" },
          { id: "tm", label: "TaskManager" },
        ],
        allowFreeText: true,
        sensitive: false,
      },
    ],
  },
  deadline: unknownKnowledge("none"),
  deadlineSource: "none",
  answer: { state: "not-applicable" },
  delivery: "not-sent",
  resolution: { state: "not-applicable" },
};

const insStarting = id("ins_");
const insExited = id("ins_");
instances.push(instanceBase(insQuestion, journalQuestion, "ready", known("waiting-interaction")));
instances.push({ ...instanceBase(insStarting, id("obj_"), "starting", unknownKnowledge("starting")), activeRunIds: [] });
instances.push({
  ...instanceBase(insExited, id("obj_"), "exited", known("idle")),
  activeRunIds: [],
  exit: known({ code: 1, signal: null, observedAt: ts }),
});
interactions.push(questionInteraction);
journals.set(journalQuestion, [
  obs(insQuestion, journalQuestion, 1, "message", {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    messageId: id("obj_"),
    role: "user",
    phase: "input",
    blocks: [{ type: "text", text: "spill 从哪改？" }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("ui"),
    status: "complete",
  }),
  obs(insQuestion, journalQuestion, 2, "interaction.requested", { interaction: questionInteraction }),
]);

const journalGrok = id("obj_");
const insGrok = id("ins_");
instances.push({
  ...instanceBase(insGrok, journalGrok, "ready", known("idle")),
  kind: "grok",
  driver: "generic-pty",
  capabilities: ptyCapabilities("generic-pty"),
  activeRunIds: [],
});
journals.set(journalGrok, []);

const titles = new Map<Id, string>([
  [insBlocked, "清一下 /tmp/coord-media"],
  [insWorking, "看 TaskManager spill 这段为啥抖"],
  [insIdle, "空闲会话"],
  [insQuestion, "spill 从哪改？"],
  [insStarting, "正在启动"],
  [insExited, "失败会话"],
  [insGrok, "Grok 会话"],
]);

const summaries = new Map<Id, string>([
  [insWorking, "Workflow wf_9f3 · phase compile"],
  [insBlocked, "等你批准 Bash"],
  [insQuestion, "AskUserQuestion · 1 题"],
]);

const hostOfflineId = id("hst_");
hosts.push({
  ...meta(hostOfflineId),
  label: "devbox-cn",
  ownerPrincipalId: id("prn_"),
  state: "offline",
  transport: { mode: "outbound-wss", endpointRef: id("obj_") },
});
const wspOffline = id("wsp_");
workspaces.push({
  ...meta(wspOffline),
  hostId: hostOfflineId,
  label: "valhalla",
  rootPath: "/home/valhalla",
  writePolicy: "workspace-write",
  canonicalRoot: known("/home/valhalla"),
});
const insPaused = id("ins_");
const paused = instanceBase(insPaused, id("obj_"), "ready", known("waiting-interaction"));
paused.hostId = hostOfflineId;
paused.workspaceId = wspOffline;
paused.connectivity = "disconnected";
instances.push(paused);
titles.set(insPaused, "离线主机上的审批");

const expiredId = id("int_");
const supersededId = id("int_");
const pausedIntId = id("int_");
const planId = id("int_");

interactions.push({
  ...pendingInteraction,
  ...meta(expiredId),
  id: expiredId,
  instanceId: insIdle,
  state: "expired",
  answerable: false,
  request: { kind: "approval", title: "Bash", description: "expired rm", toolCallId: null, actionRef: id("obj_"), options: pendingInteraction.request.kind === "approval" ? pendingInteraction.request.options : [], requestedPermissionsRef: null, inputDigest: digestPlaceholder() },
});
interactions.push({
  ...pendingInteraction,
  ...meta(supersededId),
  id: supersededId,
  instanceId: insIdle,
  state: "answer-committed",
  answerable: false,
  answer: known({
    commandId: id("cmd_"),
    actor: { principalId: id("prn_"), type: "human", deviceId: "dev_other-mac", instanceId: null },
    value: { kind: "approval", optionId: "allow-once", inputDigest: digestPlaceholder() },
    committedAt: ts,
  }),
  request: { kind: "approval", title: "Bash", description: "already answered elsewhere", toolCallId: null, actionRef: id("obj_"), options: pendingInteraction.request.kind === "approval" ? pendingInteraction.request.options : [], requestedPermissionsRef: null, inputDigest: digestPlaceholder() },
});
interactions.push({
  ...pendingInteraction,
  ...meta(pausedIntId),
  id: pausedIntId,
  instanceId: insPaused,
  hostId: hostOfflineId,
  state: "pending",
  request: { kind: "approval", title: "Bash", description: "host offline cmd", toolCallId: null, actionRef: id("obj_"), options: pendingInteraction.request.kind === "approval" ? pendingInteraction.request.options : [], requestedPermissionsRef: null, inputDigest: digestPlaceholder() },
});
interactions.push({
  ...pendingInteraction,
  ...meta(planId),
  id: planId,
  instanceId: insWorking,
  kind: "plan-review",
  request: {
    kind: "plan-review",
    title: "实施计划",
    planRef: id("obj_"),
    planRevision: "1",
    planDigest: digestPlaceholder(),
    options: [
      { id: "approve", label: "同意", effect: "allow-once", nativeValueRef: id("obj_") },
      { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: id("obj_") },
    ],
    allowFeedback: true,
  },
});
titles.set(insPaused, "离线主机上的审批");

const journalGap = "obj_mock_gap" as Id;
const journalStale = "obj_mock_stale" as Id;
const journalLong = "obj_mock_long" as Id;
const insGap = "ins_mock_gap" as Id;
const insStale = "ins_mock_stale" as Id;
const insLong = "ins_mock_long" as Id;

export const GAP_HISTORY_SEQ = 4;
export const GAP_TAIL_SEQ = 10;

function messagePayload(role: "user" | "assistant", text: string) {
  return {
    nodeId: id("obj_"),
    revision: "1",
    operation: "open",
    baseRevision: null,
    messageId: id("obj_"),
    role,
    phase: role === "user" ? "input" : "final",
    blocks: [{ type: "text", text }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known(role === "user" ? "ui" : "assistant"),
    status: "complete",
  };
}

function usagePayload() {
  return {
    usageId: id("obj_"),
    scope: "turn",
    scopeId: runWorking,
    mode: "snapshot",
    metricRevision: "1",
    inputTokens: known("100"),
    inputAccounting: "unknown",
    outputTokens: known("20"),
    reasoningTokens: unknownKnowledge("none"),
    cacheReadTokens: unknownKnowledge("none"),
    cacheWriteTokens: unknownKnowledge("none"),
    totalTokens: unknownKnowledge("none"),
    cost: known({ amount: "0.01", currency: "USD" }),
    accounting: "estimated",
    nativeFieldsRef: null,
  };
}

function gapJournalEvents(instanceId: Id, journalId: Id): Observation[] {
  const call = bashCall();
  return [
    obs(instanceId, journalId, 1, "message", messagePayload("user", "模拟缺口")),
    obs(instanceId, journalId, 2, "tool_call", call),
    obs(instanceId, journalId, 3, "thought", {
      nodeId: id("obj_"),
      revision: "1",
      operation: "open",
      baseRevision: null,
      thoughtId: id("obj_"),
      representation: "summary",
      text: "还没补到 result。",
      partIndex: 0,
      status: "complete",
    }),
    obs(instanceId, journalId, 4, "message", messagePayload("user", "继续")),
    obs(instanceId, journalId, 5, "tool_result", bashResult(call.toolCallId)),
    obs(instanceId, journalId, 6, "tool_call", {
      ...bashCall(),
      toolCallId: id("obj_"),
      toolName: known("Read"),
      displayTitle: known("Read"),
      category: "file-read",
      input: known({ file_path: "gap.cc" }),
    }),
    obs(instanceId, journalId, 7, "thought", {
      nodeId: id("obj_"),
      revision: "1",
      operation: "open",
      baseRevision: null,
      thoughtId: id("obj_"),
      representation: "summary",
      text: "补页中。",
      partIndex: 0,
      status: "complete",
    }),
    obs(instanceId, journalId, 8, "tool_call", {
      ...bashCall(),
      toolCallId: id("obj_"),
      toolName: known("Write"),
      displayTitle: known("Write"),
      category: "file-write",
      input: known({ file_path: "gap.md", content: "gap" }),
    }),
    obs(instanceId, journalId, 9, "thought", {
      nodeId: id("obj_"),
      revision: "1",
      operation: "open",
      baseRevision: null,
      thoughtId: id("obj_"),
      representation: "summary",
      text: "快齐了。",
      partIndex: 0,
      status: "complete",
    }),
    obs(instanceId, journalId, 10, "message", messagePayload("assistant", "缺口已补齐。")),
    obs(instanceId, journalId, 11, "usage", usagePayload()),
    obs(instanceId, journalId, 12, "opaque", {
      nativeType: "gap_tail",
      reason: "unmapped-fields",
      rawRef: {
        objectId: id("obj_"),
        offset: "0",
        length: "0",
        digest: digestPlaceholder(),
        mediaType: "application/json",
        redaction: "none",
      },
      affects: [],
      summary: "gap_tail",
    }),
  ];
}

instances.push({ ...instanceBase(insGap, journalGap, "ready", known("working")), activeRunIds: [] });
instances.push({ ...instanceBase(insStale, journalStale, "ready", known("working")), activeRunIds: [] });
instances.push({ ...instanceBase(insLong, journalLong, "ready", known("idle")), activeRunIds: [] });
journals.set(journalGap, gapJournalEvents(insGap, journalGap));
journals.set(journalStale, gapJournalEvents(insStale, journalStale));
journals.set(journalLong, []);
titles.set(insGap, "补页缺口会话");
titles.set(insStale, "只读缺口会话");
titles.set(insLong, LONG_SESSION_TITLE);
summaries.set(insGap, "mock gap backfill");
summaries.set(insStale, "mock fill fail");
summaries.set(insLong, `${LONG_EVENT_COUNT} events`);

const screens = new Map<Id, string[]>();

function addWorktree(label: string, branch: string, path: string) {
  const wsp = id("wsp_");
  workspaces.push({
    ...meta(wsp),
    hostId,
    label,
    rootPath: path,
    writePolicy: "isolated-worktree",
    canonicalRoot: known(path),
    worktreeLabel: label,
    branch,
  });
  return wsp;
}

function addPty(kind: Kind, driver: DriverKind, name: string, activity: Instance["activity"], workspace: Id, lines: string[]) {
  const journalId = id("obj_");
  const ins = instanceBase(id("ins_"), journalId, "ready", activity);
  ins.kind = kind;
  ins.driver = driver;
  ins.workspaceId = workspace;
  ins.nativeRef = { ...ins.nativeRef, kind };
  ins.capabilities = ptyCapabilities(driver);
  if (activity.state === "known" && activity.value === "idle") ins.activeRunIds = [];
  instances.push(ins);
  titles.set(ins.id, name);
  journals.set(journalId, []);
  screens.set(ins.id, lines);
  return ins.id;
}

const wspCodex = addWorktree("codex-worker", "wt/x-codexdrv/wire-review", "/home/dev/projects/remuda-wt/x-codexdrv");
const wspGrok = addWorktree("grok-canary", "wt/x-acpwire/canary", "/home/dev/projects/remuda-wt/x-acpwire");
const wspAgy = addWorktree("agy-board", "wt/x-design/agent-board", "/home/dev/projects/remuda-wt/x-design2");
const wspClaudePty = addWorktree("claude-pty", "wt/x-tty/push-ui", "/home/dev/projects/remuda-wt/x-tty");

export const mockBoardIds = {
  insCodexPty: addPty("codex", "generic-pty", "codex-worker", known("working"), wspCodex, [
    "reviewing hubnode hello",
    "cargo test -p remuda-hub",
    "waiting on clippy",
  ]),
  insGrokPty: addPty("grok", "generic-pty", "grok-canary", known("idle"), wspGrok, [
    "rebase onto main",
    "cargo check --workspace --locked",
    "DONE 8d3144d7ada59b21166f35d4e84f6676970aa3ad",
  ]),
  insAgyPty: addPty("agy", "generic-pty", "agy-board", known("working"), wspAgy, [
    "reading dogfood.md",
    "sketching agent board",
    "kind badge + screen snippet",
  ]),
  insClaudePty: addPty("claude", "claude-pty", "claude-pty", known("working"), wspClaudePty, [
    "attached tty",
    "waiting for send-keys",
    "ready",
  ]),
};

export function mockScreenRead(instanceId: Id, n = 3): { lines: string[] } {
  const lines = screens.get(instanceId) ?? [];
  return { lines: lines.filter((line) => line.length > 0).slice(-n) };
}

export function mockKeys(instanceId: Id, key: "enter" | "esc" | "ctrl+c"): CommandResult {
  const inst = instances.find((i) => i.id === instanceId);
  if (!inst) throw new Error("INSTANCE_NOT_FOUND");
  const lines = screens.get(instanceId) ?? [];
  lines.push(key === "enter" ? "^ENTER" : key === "esc" ? "^ESC" : "^C");
  screens.set(instanceId, lines);
  inst.updatedAt = now();
  const commandId = id("cmd_");
  return {
    command: {
      ...meta(commandId),
      commandId,
      actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId },
      origin: "ui",
      operation: "tty.write",
      target: { hostId: inst.hostId, instanceId, runId: null },
      payloadDigest: digestPlaceholder(),
      state: "accepted",
      dispatch: "intent-durable",
      resolution: "clear",
    },
    relatedCommandIds: [],
  };
}

function ensureLongJournal() {
  const cur = journals.get(journalLong);
  if (cur && cur.length >= LONG_EVENT_COUNT) return;
  journals.set(journalLong, buildLongObservations({ instanceId: insLong, journalId: journalLong, hostId }));
}

/** Lifecycles a broadcast skips; mirrors the Hub's `is_broadcast_target`. */
const BROADCAST_SKIP: ReadonlySet<string> = new Set(["exited", "failed", "closing"]);

/**
 * Mirror of `POST /v1/fleet/broadcast` over the mock instances: `all` and/or
 * `hosts` / `labels` / `kinds`, filters intersecting.
 */
export function mockFleetBroadcast(body: {
  all?: boolean;
  hosts?: string[];
  labels?: string[];
  kinds?: string[];
  operation?: "instance.send" | "tty.write";
  payload?: Record<string, unknown>;
}) {
  const operation = body.operation ?? "instance.send";
  let allowed: string[] | null = body.hosts?.length ? [...body.hosts] : null;
  if (body.labels?.length) {
    const matched = hosts
      .filter((host) => body.labels!.every((label) => (host.labels ?? []).includes(label)))
      .map((host) => host.id as string);
    allowed = allowed ? allowed.filter((id) => matched.includes(id)) : matched;
  }
  const results = [];
  let accepted = 0;
  let skipped = 0;
  for (const inst of instances) {
    if (BROADCAST_SKIP.has(inst.lifecycle)) {
      skipped += 1;
      continue;
    }
    if (allowed && !allowed.includes(inst.hostId)) {
      skipped += 1;
      continue;
    }
    if (body.kinds?.length && !body.kinds.includes(inst.kind)) {
      skipped += 1;
      continue;
    }
    accepted += 1;
    results.push({
      instanceId: inst.id as string,
      hostId: inst.hostId as string,
      kind: inst.kind as string,
      ok: true,
      commandId: id("cmd_") as string,
      state: "accepted",
      forwarded: true,
      replayed: false,
    });
  }
  return { operation, accepted, failed: 0, skipped, selected: results.length, results };
}

export const mockInstanceIds = {
  insWorking,
  insBlocked,
  insIdle,
  insQuestion,
  insStarting,
  insExited,
  insGap,
  insStale,
  insLong,
};

export const mockJournalIds = { journalGap, journalStale, journalLong };

export type MockDb = {
  hosts: Host[];
  workspaces: Workspace[];
  instances: Instance[];
  interactions: Interaction[];
  journals: Map<Id, Observation[]>;
  titles: Map<Id, string>;
  summaries: Map<Id, string>;
  permissionMode: Map<Id, string>;
};

const permissionMode = new Map<Id, string>();

export const mockDb: MockDb = { hosts, workspaces, instances, interactions, journals, titles, summaries, permissionMode };

export function mockReadJournal(journalId: Id, afterSeq?: U64, limit = 128) {
  if (journalId === journalLong) ensureLongJournal();
  const after = afterSeq ? Number(afterSeq) : 0;
  if (journalId === journalStale && after > 0) {
    throw new Error("GAP_FILL_FAILED");
  }
  const all = journals.get(journalId) ?? [];
  const truncated = (journalId === journalGap || journalId === journalStale) && after === 0;
  const source = truncated ? all.filter((e) => Number(e.seq) <= GAP_HISTORY_SEQ) : all.filter((e) => Number(e.seq) > after);
  const cap = journalId === journalLong ? Math.max(limit, LONG_EVENT_COUNT) : limit;
  const events = source.slice(0, cap);
  const durableSeq = all.length ? all[all.length - 1].seq : "0";
  return { events, durableSeq, floorSeq: "1" as U64 };
}

export function mockGappedTail(journalId: Id): EventsBatch["params"] | null {
  if (journalId !== journalGap && journalId !== journalStale) return null;
  const all = journals.get(journalId) ?? [];
  const tail = all.filter((e) => Number(e.seq) >= GAP_TAIL_SEQ);
  if (!tail.length) return null;
  return {
    subscriptionId: "sub_gap" as Id,
    journalId,
    fromSeq: tail[0].seq,
    toSeq: tail[tail.length - 1].seq,
    events: tail,
    durableSeq: all.at(-1)?.seq ?? "0",
  };
}

export function mockSnapshot(instance: Instance): Snapshot {
  if (instance.journalId === journalLong) ensureLongJournal();
  const events = journals.get(instance.journalId) ?? [];
  const gapped = instance.id === insGap || instance.id === insStale;
  const asOf = gapped ? String(GAP_HISTORY_SEQ) : events.length ? events[events.length - 1].seq : "0";
  return {
    projectionVersion: "v1",
    projectionEpoch: id("epoch_"),
    asOfSeq: asOf,
    instance,
    runs: [],
    commands: [],
    pendingInteractions: interactions.filter((i) => i.instanceId === instance.id && i.state === "pending"),
    nodes: [],
    history: { earliestRetainedSeq: "1", complete: true },
  };
}

export function mockRespond(interactionIdArg: Id, answer: InteractionAnswer): CommandResult {
  const found = interactions.find((i) => i.id === interactionIdArg);
  if (!found) throw new Error("INTERACTION_NOT_FOUND");
  const commandId = id("cmd_");
  found.state = "answer-committed";
  found.answer = known({
    commandId,
    actor: { principalId: id("prn_"), type: "human", deviceId: thisDeviceId(), instanceId: null },
    value: answer,
    committedAt: now(),
  });
  const inst = instances.find((i) => i.id === found.instanceId);
  if (inst) inst.activity = known("working");
  const journal = inst ? journals.get(inst.journalId) : undefined;
  if (inst && journal) {
    journal.push(
      obs(inst.id, inst.journalId, journal.length + 1, "interaction.answered", {
        interactionId: found.id,
        requestVersion: found.requestVersion,
        answerCommandId: commandId,
        actor: { type: "human" },
        answerRef: commandId,
        delivery: "intent-durable",
      }),
    );
  }
  const command: Command = {
    ...meta(commandId),
    commandId,
    actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId: found.instanceId },
    origin: "ui",
    operation: "interaction.respond",
    target: { hostId, instanceId: found.instanceId, runId: found.runId },
    payloadDigest: digestPlaceholder(),
    state: "accepted",
    dispatch: "intent-durable",
    resolution: "clear",
  };
  return { command, relatedCommandIds: [] };
}

export function mockSend(instanceId: Id, prompt: string): CommandResult {
  const inst = instances.find((i) => i.id === instanceId);
  if (!inst) throw new Error("INSTANCE_NOT_FOUND");
  const events = journals.get(inst.journalId) ?? [];
  events.push(
    obs(instanceId, inst.journalId, events.length + 1, "message", {
      nodeId: id("obj_"),
      revision: "1",
      operation: "open",
      baseRevision: null,
      messageId: id("obj_"),
      role: "user",
      phase: "input",
      blocks: [{ type: "text", text: prompt }],
      targetBlock: null,
      parentToolCallId: null,
      nativeOrigin: known("ui"),
      status: "complete",
    }),
  );
  journals.set(inst.journalId, events);
  inst.activity = known("working");
  inst.updatedAt = now();
  const screen = screens.get(instanceId);
  if (screen) {
    screen.push(prompt);
    screens.set(instanceId, screen);
  }
  if (!titles.get(instanceId)) titles.set(instanceId, prompt.slice(0, 80) || "新会话");
  const commandId = id("cmd_");
  const command: Command = {
    ...meta(commandId),
    commandId,
    actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId },
    origin: "ui",
    operation: "instance.send",
    target: { hostId, instanceId, runId: runWorking },
    payloadDigest: digestPlaceholder(),
    state: "accepted",
    dispatch: "intent-durable",
    resolution: "clear",
  };
  return { command, relatedCommandIds: [] };
}

const MOCK_CONFIGURE_KEY = "remuda.mock.configure";

type MockConfigurePatch = {
  permission?: string;
  model?: string;
  effort?: { name: string; index: number };
};

function readConfigurePatches(): Record<string, MockConfigurePatch> {
  if (typeof sessionStorage === "undefined") return {};
  try {
    return JSON.parse(sessionStorage.getItem(MOCK_CONFIGURE_KEY) || "{}") as Record<string, MockConfigurePatch>;
  } catch {
    return {};
  }
}

function writeConfigurePatch(instanceId: Id, patch: MockConfigurePatch) {
  if (typeof sessionStorage === "undefined") return;
  const next = { ...readConfigurePatches(), [instanceId]: { ...readConfigurePatches()[instanceId], ...patch } };
  sessionStorage.setItem(MOCK_CONFIGURE_KEY, JSON.stringify(next));
}

function applyConfigurePatches() {
  const patches = readConfigurePatches();
  for (const inst of instances) {
    const patch = patches[inst.id];
    if (!patch) continue;
    if (patch.permission) permissionMode.set(inst.id, patch.permission);
    if (patch.model) inst.model = patch.model;
    if (patch.effort) {
      inst.effortName = patch.effort.name;
      inst.effortIndex = patch.effort.index;
    }
  }
}

export function mockConfigure(
  instanceId: Id,
  permission: string,
  extras?: { model?: string; effort?: { name: string; index: number } },
): CommandResult {
  const inst = instances.find((i) => i.id === instanceId);
  if (!inst) throw new Error("INSTANCE_NOT_FOUND");
  permissionMode.set(instanceId, permission);
  if (extras?.model) inst.model = extras.model;
  if (extras?.effort) {
    inst.effortName = extras.effort.name;
    inst.effortIndex = extras.effort.index;
  }
  writeConfigurePatch(instanceId, {
    permission,
    model: extras?.model ?? inst.model ?? undefined,
    effort: extras?.effort ?? (inst.effortName != null && inst.effortIndex != null
      ? { name: inst.effortName, index: inst.effortIndex }
      : undefined),
  });
  const commandId = id("cmd_");
  const events = journals.get(inst.journalId) ?? [];
  const seq = events.length + 1;
  events.push(
    obs(inst.id, inst.journalId, seq, "lifecycle", {
      topic: "entity",
      entityType: "command",
      entityId: commandId,
      state: "accepted",
      operation: "instance.configure",
      effort: extras?.effort ?? null,
      model: extras?.model ?? null,
    }),
  );
  journals.set(inst.journalId, events);
  return {
    command: {
      ...meta(commandId),
      commandId,
      actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId },
      origin: "ui",
      operation: "instance.configure",
      target: { hostId, instanceId, runId: null },
      payloadDigest: digestPlaceholder(),
      state: "accepted",
      dispatch: "intent-durable",
      resolution: "clear",
    },
    relatedCommandIds: [],
  };
}

/**
 * Resume leaves the exited instance alone and returns a new one that continues
 * the same native session, mirroring the Hub endpoint (D-026).
 */
export function mockResume(instanceId: Id, mode: ResumeMode = "structured"): ResumeResult {
  const parent = instances.find((i) => i.id === instanceId);
  if (!parent) throw new Error("INSTANCE_NOT_FOUND");
  const driver: DriverKind = mode === "terminal" ? "claude-pty" : parent.driver;
  const existing = instances.find(
    (i) => i.parent?.instanceId === instanceId && i.driver === driver && i.lifecycle !== "exited",
  );
  if (existing) return { instanceId: existing.id, mode, replayed: true };
  const journalId = id("obj_");
  const child = instanceBase(id("ins_"), journalId, "ready", known("idle"));
  child.hostId = parent.hostId;
  child.workspaceId = parent.workspaceId;
  child.kind = parent.kind;
  child.driver = driver;
  child.cwd = parent.cwd;
  child.capabilities = driver === "claude-print" ? printCapabilities() : ptyCapabilities(driver);
  child.nativeRef = { ...parent.nativeRef };
  child.parent = { instanceId, runId: id("run_"), commandId: id("cmd_") };
  child.activeRunIds = [];
  child.updatedAt = now();
  instances.unshift(child);
  titles.set(child.id, `${titles.get(instanceId) ?? "会话"}（已恢复）`);
  journals.set(journalId, []);
  return { instanceId: child.id, mode, replayed: false };
}

export function mockClose(instanceId: Id): CommandResult {
  const inst = instances.find((i) => i.id === instanceId);
  if (!inst) throw new Error("INSTANCE_NOT_FOUND");
  inst.lifecycle = "exited";
  inst.activity = known("idle");
  inst.activeRunIds = [];
  inst.updatedAt = now();
  const commandId = id("cmd_");
  return {
    command: {
      ...meta(commandId),
      commandId,
      actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId },
      origin: "ui",
      operation: "instance.close",
      target: { hostId, instanceId, runId: null },
      payloadDigest: digestPlaceholder(),
      state: "accepted",
      dispatch: "intent-durable",
      resolution: "clear",
    },
    relatedCommandIds: [],
  };
}

/**
 * D-028 §5.3: cancel interrupts the turn only — lifecycle stays running and
 * the activity returns to idle, unlike {@link mockClose} which exits.
 */
export function mockCancel(instanceId: Id): CommandResult {
  const inst = instances.find((i) => i.id === instanceId);
  if (!inst) throw new Error("INSTANCE_NOT_FOUND");
  inst.activity = known("idle");
  inst.activeRunIds = [];
  inst.updatedAt = now();
  const commandId = id("cmd_");
  return {
    command: {
      ...meta(commandId),
      commandId,
      actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId },
      origin: "ui",
      operation: "instance.cancel",
      target: { hostId, instanceId, runId: null },
      payloadDigest: digestPlaceholder(),
      state: "accepted",
      dispatch: "intent-durable",
      resolution: "clear",
    },
    relatedCommandIds: [],
  };
}

/**
 * Mirrors the Hub's real delete: a live Instance is refused with 409 unless
 * `force` stops it first, a repeated delete is 404-idempotent, and the record
 * and its journal are gone for good.
 */
export function mockDelete(instanceId: Id, force = false): { deleted: true; instanceId: Id; nodePurge: "purged" } {
  const index = instances.findIndex((i) => i.id === instanceId);
  if (index < 0) return { deleted: true, instanceId, nodePurge: "purged" };
  const live = instances[index].lifecycle !== "exited" && instances[index].lifecycle !== "failed";
  if (live && !force) throw new HubHttpError(409, "INSTANCE_LIVE", "INSTANCE_LIVE");
  if (live) mockClose(instanceId);
  const [removed] = instances.splice(index, 1);
  journals.delete(removed.journalId);
  titles.delete(instanceId);
  summaries.delete(instanceId);
  return { deleted: true, instanceId, nodePurge: "purged" };
}

export function mockCreate(prompt: string, extras?: { hostId?: Id; workspaceId?: Id; driver?: Instance["driver"]; kind?: Instance["kind"] }): Instance {
  const journalId = id("obj_");
  const ins = instanceBase(id("ins_"), journalId, "starting", unknownKnowledge("starting"));
  if (extras?.hostId) ins.hostId = extras.hostId;
  if (extras?.workspaceId) ins.workspaceId = extras.workspaceId;
  if (extras?.driver) ins.driver = extras.driver;
  if (extras?.kind) ins.kind = extras.kind;
  if (extras?.driver === "generic-pty" || extras?.driver === "claude-pty" || extras?.driver === "shell-pty") {
    ins.capabilities = ptyCapabilities(extras.driver);
  }
  if (extras?.kind === "terminal") {
    ins.capabilities = ptyCapabilities(extras.driver ?? "shell-pty");
  }
  ins.activeRunIds = [];
  instances.unshift(ins);
  titles.set(ins.id, prompt.slice(0, 80) || "新会话");
  journals.set(journalId, [
    obs(ins.id, journalId, 1, "message", {
      nodeId: id("obj_"),
      revision: "1",
      operation: "open",
      baseRevision: null,
      messageId: id("obj_"),
      role: "user",
      phase: "input",
      blocks: [{ type: "text", text: prompt }],
      targetBlock: null,
      parentToolCallId: null,
      nativeOrigin: known("ui"),
      status: "complete",
    }),
  ]);
  return ins;
}

export function mockPage<T>(items: T[]): Page<T> {
  return { items, nextCursor: null };
}

export function mockEventsBatch(journalId: Id, fromSeq: U64, toSeq: U64): EventsBatch {
  const all = journals.get(journalId) ?? [];
  const events = all.filter((e) => Number(e.seq) >= Number(fromSeq) && Number(e.seq) <= Number(toSeq));
  return {
    jsonrpc: "2.0",
    method: "events.batch",
    params: {
      subscriptionId: id("sub_"),
      journalId,
      fromSeq,
      toSeq,
      events,
      durableSeq: all.at(-1)?.seq ?? "0",
    },
  };
}

export const mockHostName = hosts[0].label;
export const mockWorkspaceLabel = workspaces[0].label;

type MockDevice = PairedDevice & { token: string };
type MockPairRow = { code: string; expiresAt: string; used: boolean };

const DEVICES_KEY = "runtime.mock-devices";
const CODES_KEY = "runtime.mock-pair-codes";
const PASSKEYS_KEY = "runtime.mock-passkeys";
const PAIR_ALPH = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

function readJson<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : fallback;
  } catch {
    return fallback;
  }
}

function writeJson(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* ignore */
  }
}

function mockDevices(): MockDevice[] {
  return readJson<MockDevice[]>(DEVICES_KEY, []);
}

function mockPairCodes(): MockPairRow[] {
  return readJson<MockPairRow[]>(CODES_KEY, []);
}

function mockPasskeys(): MockPasskey[] {
  return readJson<MockPasskey[]>(PASSKEYS_KEY, []);
}

export function resetMockAuth(): void {
  try {
    localStorage.removeItem(DEVICES_KEY);
    localStorage.removeItem(CODES_KEY);
    localStorage.removeItem(PASSKEYS_KEY);
  } catch {
    /* ignore */
  }
}

function requireMockDevice(token: string | undefined): MockDevice {
  const found = token ? mockDevices().find((d) => d.token === token) : undefined;
  if (!found) throw new HubHttpError(401, "UNAUTHENTICATED", "UNAUTHENTICATED");
  return found;
}

function mockAuthId(prefix: string): Id {
  const rand = typeof crypto !== "undefined" && "randomUUID" in crypto ? crypto.randomUUID() : `${Date.now()}-${Math.random()}`;
  return `${prefix}${rand}` as Id;
}

export function mockLogin(bootstrapToken: string, deviceName: string): DeviceSession {
  if (bootstrapToken !== MOCK_BOOTSTRAP_TOKEN) {
    throw new HubHttpError(401, "UNAUTHENTICATED", "UNAUTHENTICATED");
  }
  const token = mockAuthId("tok_");
  const device: MockDevice = { id: mockAuthId("dev_"), name: deviceName.trim() || "device", token };
  writeJson(DEVICES_KEY, [...mockDevices(), device]);
  return { deviceId: device.id, token, name: device.name };
}

export function mockDeviceList(token: string | undefined): { items: PairedDevice[] } {
  requireMockDevice(token);
  return { items: mockDevices().map(({ id, name }) => ({ id, name })) };
}

export function mockPairCode(token: string | undefined): PairCode {
  requireMockDevice(token);
  let code = "";
  for (let i = 0; i < 8; i++) code += PAIR_ALPH[Math.floor(Math.random() * PAIR_ALPH.length)] ?? "A";
  const expiresAt = new Date(Date.now() + 10 * 60_000).toISOString();
  writeJson(CODES_KEY, [...mockPairCodes(), { code, expiresAt, used: false }]);
  return { code, expiresAt };
}

export function mockPairRedeem(code: string, deviceName: string): DeviceSession {
  const normalized = code.trim().toUpperCase();
  const rows = mockPairCodes();
  const found = rows.find((row) => row.code === normalized && !row.used);
  if (!found || Date.parse(found.expiresAt) < Date.now()) {
    throw new HubHttpError(401, "UNAUTHENTICATED", "UNAUTHENTICATED");
  }
  found.used = true;
  writeJson(CODES_KEY, rows);
  const token = mockAuthId("tok_");
  const device: MockDevice = { id: mockAuthId("dev_"), name: deviceName.trim() || "phone", token };
  writeJson(DEVICES_KEY, [...mockDevices(), device]);
  return { deviceId: device.id, token, name: device.name };
}

export function mockDeviceRevoke(token: string | undefined, deviceId: string): { ok: boolean } {
  requireMockDevice(token);
  const next = mockDevices().filter((d) => d.id !== deviceId);
  if (next.length === mockDevices().length) throw new HubHttpError(404, "NOT_FOUND", "NOT_FOUND");
  writeJson(DEVICES_KEY, next);
  return { ok: true };
}


type MockPasskeyEnvelope = { challengeId: string; options: Record<string, unknown> };

function mockChallengeId(): string {
  const rand = typeof crypto !== "undefined" && "randomUUID" in crypto ? crypto.randomUUID() : `${Date.now()}-${Math.random()}`;
  return `chal_${rand}`;
}

function mockRelyingPartyId(): string {
  try {
    return window.location.hostname || "localhost";
  } catch {
    return "localhost";
  }
}

function randomB64Url(bytes: number): string {
  const data = new Uint8Array(bytes);
  crypto.getRandomValues(data);
  let binary = "";
  for (const byte of data) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

let pendingPasskeyName = "Passkey";

export function mockPasskeyRegisterStart(name: string): MockPasskeyEnvelope {
  requireMockDevice(readSession()?.token);
  pendingPasskeyName = name.trim() || "Passkey";
  return {
    challengeId: mockChallengeId(),
    options: {
      publicKey: {
        challenge: randomB64Url(32),
        rp: { name: "Remuda", id: mockRelyingPartyId() },
        user: { id: randomB64Url(16), name: "operator", displayName: "Remuda Operator" },
        pubKeyCredParams: [{ type: "public-key", alg: -7 }, { type: "public-key", alg: -257 }],
        authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "required" },
        timeout: 60000,
        attestation: "none",
        excludeCredentials: [],
      },
    },
  };
}

export function mockPasskeyRegisterFinish(createdBy: string): MockPasskey {
  requireMockDevice(readSession()?.token);
  const record: MockPasskey = {
    id: id("psk_"),
    name: pendingPasskeyName,
    createdAt: new Date().toISOString(),
    lastUsedAt: null,
    thisDevice: true,
  };
  void createdBy;
  writeJson(PASSKEYS_KEY, [record, ...mockPasskeys()]);
  return record;
}

export function mockPasskeyLoginStart(mediation?: string): MockPasskeyEnvelope {
  const options: Record<string, unknown> = {
    publicKey: {
      challenge: randomB64Url(32),
      rpId: mockRelyingPartyId(),
      allowCredentials: [],
      userVerification: "required",
      timeout: 60000,
    },
  };
  if (mediation === "conditional") options.mediation = "conditional";
  return { challengeId: mockChallengeId(), options };
}

export function mockPasskeyLoginFinish(deviceName: string | undefined, passkeyId?: string): DeviceSession {
  const token = mockAuthId("tok_");
  const passkey = passkeyId ? mockPasskeys().find((row) => row.id === passkeyId) : mockPasskeys()[0];
  const fallback = passkey?.name ?? "passkey";
  const device: MockDevice = { id: mockAuthId("dev_"), name: deviceName?.trim() || fallback, token };
  writeJson(DEVICES_KEY, [...mockDevices(), device]);
  if (passkey) {
    writeJson(
      PASSKEYS_KEY,
      mockPasskeys().map((row) => (row.id === passkey.id ? { ...row, lastUsedAt: new Date().toISOString() } : row)),
    );
  }
  return { deviceId: device.id, token, name: device.name };
}

export function mockPasskeyList(token: string | undefined): { items: MockPasskey[] } {
  requireMockDevice(token);
  return { items: mockPasskeys().map((row) => ({ ...row, thisDevice: false })) };
}

export function mockPasskeyRename(token: string | undefined, passkeyId: string, name: string): MockPasskey {
  requireMockDevice(token);
  const rows = mockPasskeys();
  const found = rows.find((row) => row.id === passkeyId);
  if (!found) throw new HubHttpError(404, "NOT_FOUND", "NOT_FOUND");
  const updated = { ...found, name };
  writeJson(PASSKEYS_KEY, rows.map((row) => (row.id === passkeyId ? updated : row)));
  return { ...updated, thisDevice: false };
}

export function mockPasskeyDelete(token: string | undefined, passkeyId: string): { ok: boolean } {
  requireMockDevice(token);
  const rows = mockPasskeys();
  const next = rows.filter((row) => row.id !== passkeyId);
  if (next.length === rows.length) throw new HubHttpError(404, "NOT_FOUND", "NOT_FOUND");
  writeJson(PASSKEYS_KEY, next);
  return { ok: true };
}

applyConfigurePatches();
