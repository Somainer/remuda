/**
 * Map native Claude Workflow journal + task_* stream events (claude-control-plane.md §4)
 * onto observation envelopes. Source: docs/research/claude-control-plane.md §4
 * and crates/remuda-testing/fixtures/claude (workflow journal shape).
 */
import type { Observation, WorkflowMemberPayload, WorkflowRunPayload } from "../../types/generated";
import { known, unknownKnowledge, type Id } from "../../types/wire";

export type NativeWorkflowLine =
  | { type: "launched" }
  | { type: "started"; key: string; agentId: string; label: string }
  | { type: "result"; key: string; agentId: string; result: string };

export type NativeTaskEvent =
  | { type: "task_started"; task_type?: string; task_id?: string }
  | { type: "task_progress"; workflow_progress?: { state: string; agentId?: string; model?: string }[] }
  | { type: "task_updated"; status?: string }
  | { type: "task_notification"; output_file?: string }
  | { type: "background_tasks_changed" };

function envelope(
  seq: number,
  kind: Observation["kind"],
  payload: unknown,
  extras?: { completeness?: Observation["completeness"] },
): Observation {
  return {
    schemaVersion: 1,
    eventId: `evt_wf_${seq}`,
    journalId: "obj_wf_journal",
    instanceId: "ins_wf",
    runId: "run_wf",
    hostId: "hst_wf",
    processGeneration: "1",
    runGeneration: "1",
    seq: String(seq),
    observedAt: "2026-09-12T00:00:00.000Z",
    nativeAt: known("2026-09-12T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "2.1.268",
      adapterVersion: "0.1.0",
      channel: kind.startsWith("workflow") ? "workflow-journal" : "stdout",
      delivery: "replay",
      nativeSessionId: known("33333333-3333-4333-8333-333333333333"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind,
    completeness: extras?.completeness ?? "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

export function mapWorkflowJournal(lines: NativeWorkflowLine[], runId = "wf_32307b68-eda"): Observation[] {
  const workflowId = `obj_${runId}` as Id;
  const events: Observation[] = [];
  let seq = 1;
  const run: WorkflowRunPayload = {
    workflowId,
    engine: "claude-workflow",
    nativeRunId: known(runId),
    nativeTaskId: unknownKnowledge("none"),
    toolCallId: null,
    state: "queued",
    revision: "1",
    title: known("probe-ok"),
    resultRef: null,
  };
  for (const line of lines) {
    if (line.type === "launched") {
      events.push(envelope(seq++, "workflow.run", { ...run, state: "running", revision: String(seq) }));
      continue;
    }
    if (line.type === "started") {
      const member: WorkflowMemberPayload = {
        workflowId,
        memberId: `obj_${line.agentId}` as Id,
        nativeAgentId: known(line.agentId),
        nativeKey: known(line.key),
        attempt: known("1"),
        phaseId: null,
        label: known(line.label),
        state: "running",
        modelRequested: unknownKnowledge("journal"),
        modelResolved: unknownKnowledge("journal"),
        resultRef: null,
        revision: "1",
      };
      events.push(envelope(seq++, "workflow.member", member));
      continue;
    }
    if (line.type === "result") {
      events.push(
        envelope(seq++, "workflow.member", {
          workflowId,
          memberId: `obj_${line.agentId}` as Id,
          nativeAgentId: known(line.agentId),
          nativeKey: known(line.key),
          attempt: known("1"),
          phaseId: null,
          label: known(line.result),
          state: "completed",
          modelRequested: unknownKnowledge("journal"),
          modelResolved: unknownKnowledge("journal"),
          resultRef: null,
          revision: "2",
        }),
      );
      events.push(envelope(seq++, "workflow.run", { ...run, state: "completed", revision: String(seq) }));
    }
  }
  return events;
}

export function mapTaskEvents(events: NativeTaskEvent[]): Observation[] {
  return events.map((ev, i) =>
    envelope(
      i + 1,
      "opaque",
      {
        nativeType: ev.type,
        reason: "unmapped-fields",
        rawRef: {
          objectId: `obj_task_${i}`,
          offset: "0",
          length: "0",
          digest: "sha256:00",
          mediaType: "application/json",
          redaction: "none",
        },
        affects: ["presentation"],
        summary: ev.type,
      },
      { completeness: "opaque" },
    ),
  );
}
