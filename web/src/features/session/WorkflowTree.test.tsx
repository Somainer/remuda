import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload, WorkflowState } from "../../types/generated";
import { WorkflowTree } from "./WorkflowTree";

const known = <T,>(value: T) => ({ state: "known" as const, value });
const unknownK = { state: "unknown" as const, reason: "not-emitted", evidenceEventIds: [] };
const u = (n: number) => String(n);

function run(state: WorkflowRunPayload["state"] = "running"): WorkflowRunPayload {
  return {
    workflowId: "wf_demo",
    engine: "claude-workflow",
    nativeRunId: known("wf_native"),
    nativeTaskId: known("task"),
    toolCallId: "call-1",
    state,
    revision: u(1),
    title: known("demo"),
    name: known("demo-wf"),
    description: known("demo workflow"),
    totals: null,
    live: null,
    note: null,
    launchedAt: null,
    resultRef: null,
  };
}

function phase(id: string, label: string, state: WorkflowState = "running"): WorkflowPhasePayload {
  return {
    workflowId: "wf_demo",
    phaseId: id,
    nativePhaseId: known(id),
    label: known(label),
    state,
    revision: u(1),
    parentPhaseId: null,
  };
}

function member(id: string, phaseId: string, state: WorkflowMemberPayload["state"]): WorkflowMemberPayload {
  return {
    workflowId: "wf_demo",
    memberId: id,
    nativeAgentId: known(`agent-${id}`),
    nativeKey: known(`key-${id}`),
    attempt: unknownK,
    phaseId,
    label: known(id),
    state,
    modelRequested: unknownK,
    modelResolved: known("claude-opus-5"),
    resultRef: null,
    revision: u(1),
    latestTool: null,
    tokens: null,
    calls: null,
    durationMs: null,
    startedAt: null,
    endedAt: null,
    lastProgressAt: null,
  };
}

function renderTree() {
  return render(
    <MemoryRouter initialEntries={["/s/inst_test"]}>
      <WorkflowTree
        run={run("running")}
        phases={[phase("p1", "DeepRead"), phase("p2", "Verify")]}
        members={[member("a", "p1", "completed"), member("b", "p2", "running")]}
      />
    </MemoryRouter>,
  );
}

describe("WorkflowTree phase chevron", () => {
  it("starts with the run and both phases expanded", () => {
    renderTree();
    const runDetails = screen.getByTestId("workflow-tree");
    expect(runDetails).toHaveAttribute("open");
    const phases = screen.getAllByTestId("workflow-phase");
    expect(phases).toHaveLength(2);
    for (const p of phases) {
      expect(p).toHaveAttribute("open");
      expect(p.querySelector(":scope > summary")).toHaveAttribute("aria-expanded", "true");
    }
  });

  it("shows the right chevron state for a collapsed phase while the run stays open", async () => {
    const user = userEvent.setup();
    renderTree();
    const runDetails = screen.getByTestId("workflow-tree");
    const phase = screen.getAllByTestId("workflow-phase")[0];
    const summary = phase.querySelector(":scope > summary")!;

    // The chevron glyph is driven by the phase's OWN details state, so the
    // summary must expose that state (its own <details open> + aria-expanded),
    // independent of the open run <details> above it.
    await user.click(summary);
    expect(phase).not.toHaveAttribute("open");
    expect(phase).toHaveAttribute("data-open", "0");
    expect(summary).toHaveAttribute("aria-expanded", "false");
    // The ancestor run is still open — under the old descendant selector this
    // combination wrongly kept the down chevron.
    expect(runDetails).toHaveAttribute("open");

    await user.click(summary);
    expect(phase).toHaveAttribute("open");
    expect(phase).toHaveAttribute("data-open", "1");
    expect(summary).toHaveAttribute("aria-expanded", "true");
  });
});
