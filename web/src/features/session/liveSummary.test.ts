import { describe, expect, it } from "vitest";
import type { Observation } from "../../types/observation";
import type { Id } from "../../types/wire";
import { known, unknownKnowledge } from "../../types/wire";
import { liveSummary } from "./liveSummary";
function obs(kind: Observation["kind"], payload: unknown): Observation {
  return {
    schemaVersion: 1,
    eventId: `evt_${Math.random()}` as Id,
    journalId: "jrn_1" as Id,
    instanceId: "ins_1" as Id,
    runId: "run_1" as Id,
    hostId: "hst_1" as Id,
    processGeneration: "1",
    runGeneration: "1",
    seq: "1",
    observedAt: "2026-09-19T00:00:00.000Z",
    completeness: "structured",
    nativeAt: known("2026-09-19T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "2.1.268",
      adapterVersion: "0.1.0",
      channel: "stdout",
      delivery: "live",
      nativeSessionId: known("s"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind,
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

const assistant = (text: string) =>
  obs("message", { role: "assistant", blocks: [{ type: "text", text }] });

describe("liveSummary", () => {
  it("is undefined with no run or assistant line", () => {
    expect(liveSummary([])).toBeUndefined();
    expect(liveSummary([obs("tool_call", { toolName: "Bash" })])).toBeUndefined();
  });

  it("uses the latest assistant line as the phrase", () => {
    expect(liveSummary([assistant("echo: ship it")])).toBe("echo: ship it");
    const many = [assistant("first"), assistant("second")];
    expect(liveSummary(many)).toBe("second");
  });

  it("collapses a multi-line assistant message onto one line", () => {
    expect(liveSummary([assistant("line one\nline two\n")])).toBe("line one line two");
  });

  it("projects a running workflow run and its newest phase label", () => {
    const events = [
      obs("workflow.run", {
        workflowId: "obj_wf",
        state: "running",
        nativeRunId: known("wf_ab12"),
      }),
      obs("workflow.phase", {
        workflowId: "obj_wf",
        state: "running",
        label: known("compile"),
      }),
    ];
    expect(liveSummary(events)).toBe("Workflow wf_ab12 · phase compile");
  });

  it("names the run without a phase when no phase label is known", () => {
    const events = [
      obs("workflow.run", { workflowId: "obj_wf", state: "running", nativeRunId: known("wf_ab12") }),
    ];
    expect(liveSummary(events)).toBe("Workflow wf_ab12");
  });

  it("keeps a native run id longer than 8 characters intact (the short-code rule is for ins_ ids)", () => {
    const events = [
      obs("workflow.run", { workflowId: "obj_wf", state: "running", nativeRunId: known("wf-native-demo") }),
      obs("workflow.phase", { workflowId: "obj_wf", state: "running", label: known("Review") }),
    ];
    expect(liveSummary(events)).toBe("Workflow wf-native-demo · phase Review");
  });

  it("falls back to the assistant tail once the workflow has finished", () => {
    const events = [
      obs("workflow.run", { workflowId: "obj_wf", state: "running", nativeRunId: known("wf_ab12") }),
      obs("workflow.phase", { workflowId: "obj_wf", state: "running", label: known("compile") }),
      obs("workflow.run", { workflowId: "obj_wf", state: "completed", nativeRunId: known("wf_ab12") }),
      assistant("all done"),
    ];
    expect(liveSummary(events)).toBe("all done");
  });

  it("names the active running phase, not a queued phase that follows it", () => {
    const events = [
      obs("workflow.run", { workflowId: "obj_wf", state: "running", nativeRunId: known("wf_ab12") }),
      obs("workflow.phase", { workflowId: "obj_wf", state: "running", label: known("Review") }),
      obs("workflow.phase", { workflowId: "obj_wf", state: "queued", label: known("Verify") }),
    ];
    expect(liveSummary(events)).toBe("Workflow wf_ab12 · phase Review");
  });

  it("ignores a phase belonging to a different workflow", () => {
    const events = [
      obs("workflow.run", { workflowId: "obj_wf", state: "running", nativeRunId: known("wf_ab12") }),
      obs("workflow.phase", { workflowId: "obj_other", state: "running", label: known("compile") }),
    ];
    expect(liveSummary(events)).toBe("Workflow wf_ab12");
  });
});
