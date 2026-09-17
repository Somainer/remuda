import { describe, expect, it } from "vitest";
import { assembleTranscript, collectTasks, compactTranscript } from "./assemble";
import { mapTaskEvents, mapWorkflowJournal, type NativeTaskEvent, type NativeWorkflowLine } from "./workflowMap";
import { usageLine } from "./usage";
import { known, unknownKnowledge } from "../../types/wire";
import type { UsagePayload } from "../../types/generated";
import workflowRaw from "../../fixtures/claude/workflow.jsonl?raw";
import taskRaw from "../../fixtures/claude/task-events.jsonl?raw";

function jsonl<T>(raw: string): T[] {
  return raw
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line) as T);
}

describe("workflow journal fixture (control-plane §4)", () => {
  it("maps launched/started/result onto run → member tree", () => {
    const lines = jsonl<NativeWorkflowLine>(workflowRaw);
    const events = mapWorkflowJournal(lines);
    const nodes = assembleTranscript(events);
    const wf = nodes.find((n) => n.type === "workflow");
    expect(wf?.type).toBe("workflow");
    if (wf?.type !== "workflow") return;
    expect(wf.run.state).toBe("completed");
    expect(wf.members.some((m) => m.state === "completed")).toBe(true);
  });
});

describe("task_* events fixture", () => {
  it("keeps native task_* as opaque rows", () => {
    const lines = jsonl<NativeTaskEvent>(taskRaw);
    const events = mapTaskEvents(lines);
    const nodes = assembleTranscript(events);
    expect(nodes.filter((n) => n.type === "opaque").map((n) => (n.type === "opaque" ? n.kind : ""))).toEqual([
      "background_tasks_changed",
      "task_started",
      "task_progress",
      "task_updated",
      "task_notification",
    ]);
  });
});

describe("usageLine", () => {
  const base: UsagePayload = {
    usageId: "obj_u",
    scope: "turn",
    scopeId: "run",
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

  it("hides the row when input or output is unknown", () => {
    expect(usageLine(base)).toContain("in 100");
    expect(usageLine({ ...base, inputTokens: unknownKnowledge("none") })).toBeNull();
    expect(usageLine({ ...base, outputTokens: unknownKnowledge("none") })).toBeNull();
    expect(usageLine({ ...base, cost: unknownKnowledge("none") })).toContain("out 20");
    expect(usageLine({ ...base, cost: unknownKnowledge("none") })).not.toContain("$");
  });
});

describe("collectTasks", () => {
  it("lifts Task family out of compact folds", () => {
    const events = mapWorkflowJournal(jsonl<NativeWorkflowLine>(workflowRaw));
    expect(collectTasks(compactTranscript(assembleTranscript(events), true))).toEqual([]);
  });
});
