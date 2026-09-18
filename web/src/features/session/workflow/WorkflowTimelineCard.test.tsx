import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload, WorkflowState } from "../../../types/generated";
import { WorkflowTimelineCard } from "./WorkflowTimelineCard";

function renderCard(element: React.ReactElement) {
  return render(<MemoryRouter initialEntries={["/s/inst_test"]}>{element}</MemoryRouter>);
}

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
    launchedAt: partial.launchedAt ?? null,
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
    startedAt: m.startedAt ?? null,
    endedAt: m.endedAt ?? null,
    lastProgressAt: m.lastProgressAt ?? null,
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
    renderCard(
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

  it("stays open through the terminal transition; only dismiss collapses it", async () => {
    const user = userEvent.setup();
    const onDismiss = vi.fn();
    const onUndismiss = vi.fn();
    const members = [
      member({ memberId: "a", state: "running", label: known("a"), durationMs: u(130_000), tokens: u(48_000) }),
      member({ memberId: "b", state: "running", label: known("b"), durationMs: u(108_000), tokens: u(39_000) }),
    ];
    const element = (dismissed: boolean, state: WorkflowState = "running") => (
      <WorkflowTimelineCard
        run={run({ state })}
        phases={[state === "running" ? phase() : phase("p1", "Review", "completed")]}
        members={state === "running" ? members : members.map((m) => ({ ...m, state: "completed" }))}
        dismissed={dismissed}
        onDismiss={onDismiss}
        onUndismiss={onUndismiss}
      />
    );
    const { rerender } = renderCard(element(false));
    const head = () => screen.getByTestId("workflow-card").querySelector("button")!;
    expect(head()).toHaveAttribute("aria-expanded", "true");

    // Terminal transition must NOT auto-collapse.
    rerender(
      <MemoryRouter initialEntries={["/s/inst_test"]}>{element(false, "completed")}</MemoryRouter>,
    );
    expect(head()).toHaveAttribute("aria-expanded", "true");

    // Dismissal is the only thing that closes the card: clicking the head
    // reports onDismiss, and the persisted dismissed prop then collapses it.
    await user.click(head());
    expect(onDismiss).toHaveBeenCalledOnce();
    rerender(
      <MemoryRouter initialEntries={["/s/inst_test"]}>{element(true, "completed")}</MemoryRouter>,
    );
    expect(head()).toHaveAttribute("aria-expanded", "false");
    expect(head().textContent).toContain("2/2 agents");

    // Re-opening reports onUndismiss and expands again.
    await user.click(head());
    expect(onUndismiss).toHaveBeenCalledOnce();
    rerender(
      <MemoryRouter initialEntries={["/s/inst_test"]}>{element(false, "completed")}</MemoryRouter>,
    );
    expect(head()).toHaveAttribute("aria-expanded", "true");
  });

  it("renders per-agent duration, idle, queue and tokens with a live clock", async () => {
    const t = new Date("2026-09-18T12:00:40.000Z").getTime();
    vi.useFakeTimers();
    vi.setSystemTime(t);
    try {
      renderCard(
        <WorkflowTimelineCard
          run={run({ launchedAt: "2026-09-18T12:00:00.000Z" })}
          phases={[phase()]}
          members={[
            member({
              memberId: "a",
              label: known("review:security"),
              state: "running",
              tokens: u(21_000),
              startedAt: "2026-09-18T12:00:10.000Z",
              lastProgressAt: "2026-09-18T12:00:30.000Z",
            }),
          ]}
        />,
      );
      const row = screen.getByTestId("workflow-agent");
      expect(within(row).getByTestId("workflow-agent-duration").textContent).toContain("0m 30");
      expect(within(row).getByTestId("workflow-agent-idle").textContent).toContain("0m 10");
      expect(within(row).getByTestId("workflow-agent-queue").textContent).toContain("0m 10");
      expect(within(row).getByTestId("workflow-agent-tokens").textContent).toContain("21k");
      // The one-second hand advances duration and idle, not the queue wait.
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      expect(within(row).getByTestId("workflow-agent-duration").textContent).toContain("0m 32");
      expect(within(row).getByTestId("workflow-agent-idle").textContent).toContain("0m 12");
      expect(within(row).getByTestId("workflow-agent-queue").textContent).toContain("0m 10");
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows the em dash for missing metrics, never a zero", () => {
    renderCard(
      <WorkflowTimelineCard
        run={run()}
        phases={[phase()]}
        members={[member({ memberId: "a", label: known("review:security"), state: "running" })]}
      />,
    );
    const row = screen.getByTestId("workflow-agent");
    expect(within(row).getByTestId("workflow-agent-duration").textContent).toContain("—");
    expect(within(row).getByTestId("workflow-agent-idle").textContent).toContain("—");
    expect(within(row).getByTestId("workflow-agent-queue").textContent).toContain("—");
    expect(within(row).getByTestId("workflow-agent-tokens").textContent).toContain("—");
  });

  it("renders killed as 已终止", () => {
    renderCard(
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
    renderCard(<WorkflowTimelineCard run={run({ state: "completed" })} phases={[phase()]} members={agents} />);
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
    renderCard(<WorkflowTimelineCard run={run({ state: "failed" })} phases={[phase()]} members={agents} />);
    await openCardAndPhase();
    expect(screen.getByText("review:boom")).toBeTruthy();
    const failedRow = screen.getByText("review:boom").closest("[data-state]")!;
    expect(failedRow).toHaveAttribute("data-state", "failed");
  });

  it("shows the attempt marker as ×2", async () => {
    renderCard(
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
    renderCard(<WorkflowTimelineCard run={run({ note: "daemon 版本较旧，暂无阶段明细" })} phases={[]} members={[]} />);
    const card = screen.getByTestId("workflow-card-flat");
    expect(card.textContent).toContain("阶段明细");
    expect(within(card).queryByTestId("workflow-agent")).toBeNull();
  });
});
