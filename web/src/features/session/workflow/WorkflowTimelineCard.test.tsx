import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload, WorkflowState } from "../../../types/generated";
import { WorkflowTimelineCard } from "./WorkflowTimelineCard";

const known = <T,>(value: T) => ({ state: "known" as const, value });
const unknownK = { state: "unknown" as const, reason: "not-emitted", evidenceEventIds: [] };
const u = (n: number) => String(n);

function run(partial: Partial<WorkflowRunPayload> = {}): WorkflowRunPayload {
  return {
    workflowId: "wf_demo",
    engine: "claude-workflow",
    nativeRunId: known("wf_native"),
    nativeTaskId: known("task"),
    toolCallId: "call-1",
    state: (partial.state ?? "running") as WorkflowState,
    revision: u(1),
    title: known("demo"),
    name: partial.name ?? known("demo-wf"),
    description: known("demo workflow"),
    totals: partial.totals ?? null,
    live: partial.live ?? null,
    note: partial.note ?? null,
    resultRef: null,
  };
}

function phase(id = "p1", label = "Review", state: WorkflowState = "running"): WorkflowPhasePayload {
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

function member(m: Partial<WorkflowMemberPayload> & { memberId: string }): WorkflowMemberPayload {
  return {
    workflowId: "wf_demo",
    memberId: m.memberId,
    nativeAgentId: known(`agent-${m.memberId}`),
    nativeKey: known(`key-${m.memberId}`),
    attempt: m.attempt ?? unknownK,
    phaseId: m.phaseId ?? "p1",
    label: m.label ?? known(`label-${m.memberId}`),
    state: m.state ?? "running",
    modelRequested: unknownK,
    modelResolved: m.modelResolved ?? known("claude-opus-5"),
    resultRef: null,
    revision: u(1),
    latestTool: m.latestTool ?? null,
    tokens: m.tokens ?? null,
    calls: m.calls ?? null,
    durationMs: m.durationMs ?? null,
    startedAt: null,
    endedAt: null,
  };
}

/** Expand the card then its first (collapsed, completed) phase. */
async function openCardAndPhase() {
  const user = userEvent.setup();
  const card = screen.getByTestId("workflow-card");
  const head = card.querySelector("button")!;
  if (head.getAttribute("aria-expanded") !== "true") await user.click(head);
  const phaseHead = screen.getByTestId("workflow-phase").querySelector("button")!;
  if (phaseHead.getAttribute("aria-expanded") !== "true") await user.click(phaseHead);
  return user;
}

describe("WorkflowTimelineCard", () => {
  it("expands while running and shows the live line", () => {
    render(
      <WorkflowTimelineCard
        run={run()}
        phases={[phase()]}
        members={[member({ memberId: "a", label: known("review:security") })]}
      />,
    );
    const card = screen.getByTestId("workflow-card");
    expect(card).toHaveAttribute("data-status", "running");
    expect(screen.getByText("当前")).toBeTruthy();
    expect(card.textContent).toContain("Review: review:security");
  });

  it("auto-collapses when the run snapshot becomes completed and expands on click", async () => {
    const user = userEvent.setup();
    const members = [
      member({ memberId: "a", state: "running", label: known("a"), durationMs: u(130_000), tokens: u(48_000) }),
      member({ memberId: "b", state: "running", label: known("b"), durationMs: u(108_000), tokens: u(39_000) }),
    ];
    const { rerender } = render(<WorkflowTimelineCard run={run()} phases={[phase()]} members={members} />);
    const card = screen.getByTestId("workflow-card");
    expect(card.querySelector("button")).toHaveAttribute("aria-expanded", "true");
    rerender(
      <WorkflowTimelineCard
        run={run({ state: "completed" })}
        phases={[phase("p1", "Review", "completed")]}
        members={members.map((m) => ({ ...m, state: "completed" }))}
      />,
    );
    // Collapsed: header summary shows agents.
    const head = screen.getByTestId("workflow-card").querySelector("button")!;
    expect(head).toHaveAttribute("aria-expanded", "false");
    expect(head.textContent).toContain("2/2 agents");
    await user.click(head);
    expect(head).toHaveAttribute("aria-expanded", "true");
  });

  it("renders killed as 已终止", () => {
    render(
      <WorkflowTimelineCard
        run={run({ state: "cancelled" })}
        phases={[phase("p1", "Verify", "cancelled")]}
        members={[member({ memberId: "a", state: "cancelled", label: known("v:db") })]}
      />,
    );
    expect(screen.getByTestId("workflow-card").textContent).toContain("已终止");
  });

  it("folds the quiet tail behind 还有 n 个 and expands it", async () => {
    const agents = Array.from({ length: 20 }, (_, i) =>
      member({ memberId: `m${i}`, label: known(`gen:${String(i).padStart(2, "0")}`), state: "completed" }),
    );
    render(<WorkflowTimelineCard run={run({ state: "completed" })} phases={[phase()]} members={agents} />);
    await openCardAndPhase();
    const toggle = screen.getByText(/还有 8 个/);
    const rowsBefore = screen.getAllByTestId("workflow-agent");
    expect(rowsBefore).toHaveLength(12);
    await userEvent.setup().click(toggle);
    expect(screen.getAllByTestId("workflow-agent")).toHaveLength(20);
  });

  it("never folds a failed row", async () => {
    const agents = [
      ...Array.from({ length: 14 }, (_, i) => member({ memberId: `d${i}`, label: known(`d:${i}`), state: "completed" })),
      member({ memberId: "boom", label: known("review:boom"), state: "failed" }),
    ];
    render(<WorkflowTimelineCard run={run({ state: "failed" })} phases={[phase()]} members={agents} />);
    await openCardAndPhase();
    expect(screen.getByText("review:boom")).toBeTruthy();
    const failedRow = screen.getByText("review:boom").closest("[data-state]")!;
    expect(failedRow).toHaveAttribute("data-state", "failed");
  });

  it("shows the attempt marker as ×2", async () => {
    render(
      <WorkflowTimelineCard
        run={run({ state: "completed" })}
        phases={[phase()]}
        members={[member({ memberId: "a", state: "completed", label: known("v:db"), attempt: known(u(2)) })]}
      />,
    );
    await openCardAndPhase();
    expect(screen.getByText("×2")).toBeTruthy();
  });

  it("degrades to a flat row with a note when there is no detail", () => {
    render(<WorkflowTimelineCard run={run({ note: "daemon 版本较旧，暂无阶段明细" })} phases={[]} members={[]} />);
    const card = screen.getByTestId("workflow-card-flat");
    expect(card.textContent).toContain("阶段明细");
    expect(within(card).queryByTestId("workflow-agent")).toBeNull();
  });
});
