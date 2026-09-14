import type { Observation } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";

/**
 * Batch E (in-transcript search / reading position) synthetic fixture.
 *
 * 2,000 journal events: a long alternating conversation plus a deterministic
 * tail the e2e spec relies on. Everything is synthetic — no real model output
 * is ever involved. The plain {@link buildLongObservations} fixture is left
 * untouched; this one adds the surfaces batch E has to prove:
 *
 * - two unique markers loaded far outside the initial virtual window;
 * - a `Task` call with a long prompt (TaskTrack truncation);
 * - a `failed` and a `denied` tool (must stay visible, never folded away);
 * - a streaming-status assistant node (live-region boundary).
 */
export const BATCH_E_EVENT_COUNT = 2000;
export const BATCH_E_TITLE = "batch-e 搜索与阅读位置";
export const BATCH_E_MARKER_FAR = "ESEARCH-NEEDLE-FAR-7741";
export const BATCH_E_MARKER_MID = "ESEARCH-NEEDLE-MID-8852";
export const BATCH_E_FAILED_CALL = "tc_batch_e_failed";
export const BATCH_E_DENIED_CALL = "tc_batch_e_denied";
export const BATCH_E_TASK_CALL = "tc_batch_e_task";
export const BATCH_E_TASK_PROMPT = "重构会话组装层并保证稳定身份，".repeat(12);

export function buildBatchEObservations(opts: {
  instanceId: Id;
  journalId: Id;
  hostId: Id;
  count?: number;
}): Observation[] {
  const count = opts.count ?? BATCH_E_EVENT_COUNT;
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
  const push = (seq: number, kind: Observation["kind"], payload: unknown) => {
    events.push({
      schemaVersion: 1,
      eventId: `evt_batch_e_${seq}` as Id,
      journalId: opts.journalId,
      instanceId: opts.instanceId,
      runId: null,
      hostId: opts.hostId,
      processGeneration: "1",
      runGeneration: null,
      seq: String(seq),
      observedAt: ts,
      nativeAt: known(ts),
      source,
      kind,
      completeness: "structured",
      rawRef: null,
      evidenceEventIds: [],
      payload,
    } as Observation);
  };
  const message = (
    seq: number,
    role: "user" | "assistant",
    text: string,
    status: "streaming" | "complete" = "complete",
  ) => {
    const user = role === "user";
    push(seq, "message", {
      nodeId: `obj_batch_e_n_${seq}` as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      messageId: `obj_batch_e_m_${seq}` as Id,
      role,
      phase: user ? "input" : "final",
      blocks: [{ type: "text", text }],
      targetBlock: null,
      parentToolCallId: null,
      nativeOrigin: known(user ? "ui" : "assistant"),
      status,
    });
  };
  const call = (seq: number, toolCallId: string, name: string, category: string, input: unknown) => {
    push(seq, "tool_call", {
      nodeId: `obj_batch_e_nc_${seq}` as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      toolCallId: toolCallId as Id,
      parentToolCallId: null,
      toolName: known(name),
      displayTitle: known(name),
      category,
      input: known(input),
      inputTextDelta: null,
      state: "running",
      executor: known({ hostId: opts.hostId, workspaceId: null, nativeAgentId: null }),
    });
  };
  const result = (seq: number, toolCallId: string, outcome: "succeeded" | "failed" | "denied", text: string) => {
    push(seq, "tool_result", {
      nodeId: `obj_batch_e_nr_${seq}` as Id,
      revision: "1",
      operation: "close",
      baseRevision: null,
      toolCallId: toolCallId as Id,
      stage: "final",
      outcome,
      blocks: text ? [{ type: "text", text }] : [],
      structuredResult: unknownKnowledge("text"),
      exitCode: known(outcome === "succeeded" ? 0 : 1),
      changes: [],
    });
  };

  // Events 1..1990: the long alternating conversation.
  for (let i = 1; i <= 1990; i += 1) {
    const user = i % 2 === 1;
    let text = user ? `prompt ${i}` : `reply ${i}`;
    if (i === 7) text = `${text} ${BATCH_E_MARKER_FAR}`;
    if (i === 1001) text = `${text} ${BATCH_E_MARKER_MID}`;
    // The last assistant node before the tail sequence is still streaming:
    // a transcript rendering live text must not touch the live region.
    message(i, user ? "user" : "assistant", text, !user && i === 1990 ? "streaming" : "complete");
  }
  // Tail (1991..2000): task tool, routine tool, FAILED tool, turn end, then a
  // DENIED tool in the next in-flight group.
  message(1991, "user", "跑个子任务并检查测试");
  call(1992, BATCH_E_TASK_CALL, "Task", "agent", { prompt: BATCH_E_TASK_PROMPT, description: "batch-e 子任务" });
  result(1993, BATCH_E_TASK_CALL, "succeeded", "subtask done");
  call(1994, BATCH_E_FAILED_CALL, "Bash", "shell", { command: "make test" });
  result(1995, BATCH_E_FAILED_CALL, "failed", "2 tests failed");
  message(1996, "assistant", "子任务完成，但有测试失败了");
  message(1997, "user", "再试一次");
  call(1998, BATCH_E_DENIED_CALL, "Bash", "shell", { command: "make deploy" });
  result(1999, BATCH_E_DENIED_CALL, "denied", "permission denied");
  push(2000, "usage", {
    usageId: "obj_batch_e_usage" as Id,
    scope: "turn",
    scopeId: "obj_batch_e_run" as Id,
    mode: "snapshot",
    metricRevision: "1",
    inputTokens: known("1200"),
    inputAccounting: "unknown",
    outputTokens: known("340"),
    reasoningTokens: unknownKnowledge("none"),
    cacheReadTokens: unknownKnowledge("none"),
    cacheWriteTokens: unknownKnowledge("none"),
    totalTokens: unknownKnowledge("none"),
    cost: known({ amount: "0.02", currency: "USD" }),
    accounting: "estimated",
    nativeFieldsRef: null,
  });

  if (count !== BATCH_E_EVENT_COUNT) {
    // Kept deterministic if a caller ever asks for a shorter journal; the
    // tail markers are not relocated.
    return events.slice(0, count);
  }
  return events;
}
