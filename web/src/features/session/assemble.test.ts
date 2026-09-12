import { describe, expect, it } from "vitest";
import type { Observation, ToolCallPayload } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import { assembleTranscript, compactTranscript, diffState } from "./assemble";

function obs(seq: number, kind: Observation["kind"], payload: unknown): Observation {
  return {
    schemaVersion: 1,
    eventId: `evt_${seq}` as Id,
    journalId: "obj_j" as Id,
    instanceId: "ins" as Id,
    runId: null,
    hostId: "hst" as Id,
    processGeneration: "1",
    runGeneration: null,
    seq: String(seq),
    observedAt: "2026-09-12T00:00:00.000Z",
    nativeAt: known("2026-09-12T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "1",
      adapterVersion: "1",
      channel: "stdout",
      delivery: "replay",
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind,
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

function call(name: string, toolCallId: string): ToolCallPayload {
  return {
    nodeId: "n" as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: toolCallId as Id,
    parentToolCallId: null,
    toolName: known(name),
    displayTitle: known(name),
    category: "other",
    input: known({}),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
  };
}

describe("assembleTranscript", () => {
  it("pairs tool call/result and keeps opaque + workflow phase", () => {
    const events = [
      obs(1, "message", {
        nodeId: "n" as Id,
        revision: "1",
        operation: "open",
        baseRevision: null,
        messageId: "m" as Id,
        role: "user",
        phase: "input",
        blocks: [{ type: "text", text: "hi" }],
        targetBlock: null,
        parentToolCallId: null,
        nativeOrigin: known("ui"),
        status: "complete",
      }),
      obs(2, "tool_call", call("Bash", "c1")),
      obs(3, "tool_result", {
        nodeId: "n" as Id,
        revision: "1",
        operation: "close",
        baseRevision: null,
        toolCallId: "c1" as Id,
        stage: "final",
        outcome: "succeeded",
        blocks: [{ type: "text", text: "ok" }],
        structuredResult: unknownKnowledge("text"),
        exitCode: known(0),
        changes: [],
      }),
      obs(4, "workflow.run", {
        workflowId: "wf" as Id,
        engine: "claude-workflow",
        nativeRunId: known("wf_9f3"),
        nativeTaskId: unknownKnowledge("none"),
        toolCallId: null,
        state: "running",
        revision: "1",
        title: known("compile"),
        resultRef: null,
      }),
      obs(5, "workflow.phase", {
        workflowId: "wf" as Id,
        phaseId: "ph" as Id,
        nativePhaseId: known("compile"),
        label: known("compile"),
        state: "running",
        revision: "1",
        parentPhaseId: null,
      }),
      obs(6, "opaque", {
        nativeType: "rate_limit_event",
        reason: "unmapped-native",
        rawRef: {
          objectId: "o" as Id,
          offset: "0",
          length: "0",
          digest: "sha256:00",
          mediaType: "application/json",
          redaction: "none",
        },
        affects: [],
        summary: "rate_limit_event",
      }),
    ];
    const nodes = assembleTranscript(events);
    expect(nodes.map((n) => n.type)).toEqual(["message", "tool", "workflow", "opaque"]);
    const tool = nodes.find((n) => n.type === "tool");
    expect(tool?.type === "tool" && tool.result?.outcome).toBe("succeeded");
    const wf = nodes.find((n) => n.type === "workflow");
    expect(wf?.type === "workflow" && wf.phases[0]?.label.state === "known" && wf.phases[0].label.value).toBe("compile");
    const opaque = nodes.find((n) => n.type === "opaque");
    expect(opaque?.type === "opaque" && opaque.kind).toBe("rate_limit_event");
  });
});

describe("compactTranscript", () => {
  it("folds thought+tools after the assistant turn, not while in flight", () => {
    const thought = { type: "thought" as const, id: "t", text: "x", completeness: "structured" as const };
    const tool = {
      type: "tool" as const,
      id: "c",
      family: "Bash" as const,
      name: "Bash",
      driverKind: "claude-print",
      call: call("Bash", "c"),
      result: null,
      completeness: "structured" as const,
      diffState: "unknown" as const,
    };
    const inflight = compactTranscript(
      [
        { type: "message", id: "u", role: "user", text: "q", status: "complete" },
        thought,
        tool,
      ],
      true,
    );
    expect(inflight.some((n) => n.type === "compact")).toBe(false);
    const done = compactTranscript(
      [
        { type: "message", id: "u", role: "user", text: "q", status: "complete" },
        thought,
        tool,
        { type: "message", id: "a", role: "assistant", text: "ok", status: "complete" },
      ],
      true,
    );
    expect(done.some((n) => n.type === "compact")).toBe(true);
    expect(done.some((n) => n.type === "message" && n.role === "assistant")).toBe(true);
  });
});

describe("diffState", () => {
  const base = call("Edit", "e");
  it("proposed / applied / unknown", () => {
    expect(diffState({ ...base, state: "proposed" }, null, "structured")).toBe("proposed");
    expect(
      diffState(base, {
        nodeId: "n" as Id,
        revision: "1",
        operation: "close",
        baseRevision: null,
        toolCallId: "e" as Id,
        stage: "final",
        outcome: "succeeded",
        blocks: [],
        structuredResult: unknownKnowledge("diff"),
        exitCode: { state: "not-applicable" },
        changes: [{ path: "a", diff: "", application: "applied" }],
      }, "structured"),
    ).toBe("applied");
    expect(diffState(base, null, "partial")).toBe("unknown");
  });
});
