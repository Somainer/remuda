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

  it("marks an all-done live run 4/4+ provisional, never a final 完成 count", () => {
    // Dynamic run between spawning iterations: every current member completed
    // while the run itself is still running. The head count must stay
    // provisional (4/4+) so it can neither read as finished nor freeze at 100%.
    renderCard(
      <WorkflowTimelineCard
        run={run({ state: "running" })}
        phases={[phase("p1", "Review", "completed")]}
        members={[
          member({ memberId: "a", state: "completed" }),
          member({ memberId: "b", state: "completed" }),
          member({ memberId: "c", state: "completed" }),
          member({ memberId: "d", state: "completed" }),
        ]}
      />,
    );
    const count = screen.getByTestId("workflow-rail-count");
    expect(count).toHaveAttribute("data-provisional", "1");
    expect(count.textContent).toBe("4/4+ agents");
    expect(count.textContent).not.toContain("完成");
    expect(screen.getByTestId("workflow-rail")).toHaveAttribute("data-total", "provisional");
    // The completed-looking phase is provisional on the same rule.
    expect(screen.getByText("4/4+")).toBeTruthy();
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

  const elapsedTotals = (elapsedMs: string) => ({
    totalKnown: true,
    agentsTotal: u(1),
    agentsDone: u(0),
    agentsFailed: u(0),
    agentsKilled: u(0),
    agentsRunning: u(1),
    tokens: u(21_000),
    calls: u(2),
    elapsedMs,
  });

  it("reflects the phase disclosure state in aria-expanded and flips on toggle", async () => {
    const user = userEvent.setup();
    renderCard(<WorkflowTimelineCard run={run({ state: "completed" })} phases={[phase()]} members={[member({ memberId: "a", state: "completed" })]} />);
    const phaseHead = screen.getByTestId("workflow-phase").querySelector("button")!;
    // Completed phases start collapsed; the chevron is drawn right-pointing
    // (rotation is CSS off aria-expanded — the attribute is the contract).
    expect(phaseHead).toHaveAttribute("aria-expanded", "false");
    expect(phaseHead.querySelector("svg")).not.toBeNull();
    await user.click(phaseHead);
    expect(phaseHead).toHaveAttribute("aria-expanded", "true");
    await user.click(phaseHead);
    expect(phaseHead).toHaveAttribute("aria-expanded", "false");
  });

  const totalsBlock = (
    p: Partial<{ done: number; failed: number; killed: number; running: number; total: number; known: boolean }>,
  ) => ({
    totalKnown: p.known ?? true,
    agentsTotal: u(p.total ?? 0),
    agentsDone: u(p.done ?? 0),
    agentsFailed: u(p.failed ?? 0),
    agentsKilled: u(p.killed ?? 0),
    agentsRunning: u(p.running ?? 0),
    tokens: u(0),
    calls: u(0),
    elapsedMs: u(0),
  });

  it("advances the header progress from 0/8 to 8/8", () => {
    const members8 = Array.from({ length: 8 }, (_, i) => member({ memberId: `a${i}`, state: "completed" }));
    const element = (state: "running" | "completed") => (
      <WorkflowTimelineCard
        run={run({ state, totals: totalsBlock(state === "running" ? { done: 0, running: 1, total: 8 } : { done: 8, total: 8 }) })}
        phases={[phase("p1", "DeepRead", state === "running" ? "running" : "completed")]}
        members={
          state === "running"
            ? [member({ memberId: "a0", state: "running" }), ...Array.from({ length: 7 }, (_, i) => member({ memberId: `q${i}`, state: "queued" }))]
            : members8
        }
      />
    );
    const { rerender } = renderCard(element("running"));
    const count = () => screen.getByTestId("workflow-rail-count").textContent;
    expect(count()).toContain("0/8");
    expect(screen.getByTestId("workflow-rail").querySelector("[class*='railFill']")).toBeTruthy();

    rerender(<MemoryRouter initialEntries={["/s/inst_test"]}>{element("completed")}</MemoryRouter>);
    expect(count()).toContain("8/8");
  });

  it("marks a failed member in the rail and with a 已失败 count", () => {
    renderCard(
      <WorkflowTimelineCard
        run={run({ state: "failed", totals: totalsBlock({ done: 1, failed: 1, total: 2, known: false }) })}
        phases={[phase("p1", "Review", "failed")]}
        members={[
          member({ memberId: "a", state: "completed" }),
          member({ memberId: "boom", label: known("review:boom"), state: "failed" }),
        ]}
      />,
    );
    expect(screen.getByTestId("workflow-rail-count").textContent).toContain("2/2");
    const failed = screen.getByTestId("workflow-rail-failed");
    expect(failed.textContent).toContain("1 已失败");
    // Failed members are finished: full fill with a red slice over half of it.
    const failSlice = screen.getByTestId("workflow-rail").querySelector("[class*='railFail']") as HTMLElement;
    expect(failSlice.style.width).toBe("50%");
  });

  it("marks a dynamic run's total provisional while running and drops the mark at the end", () => {
    const element = (state: "running" | "completed") => (
      <WorkflowTimelineCard
        run={run({ state, totals: totalsBlock(state === "running" ? { done: 2, running: 2, total: 4, known: false } : { done: 4, total: 4, known: false }) })}
        phases={[phase("p1", "Review", state === "running" ? "running" : "completed")]}
        members={
          state === "running"
            ? [
                member({ memberId: "a", state: "completed" }),
                member({ memberId: "b", state: "completed" }),
                member({ memberId: "c", state: "running" }),
                member({ memberId: "d", state: "running" }),
              ]
            : ["a", "b", "c", "d"].map((id) => member({ memberId: id, state: "completed" }))
        }
      />
    );
    const { rerender } = renderCard(element("running"));
    const countEl = () => screen.getByTestId("workflow-rail-count");
    expect(countEl().textContent).toContain("2/4+");
    expect(countEl()).toHaveAttribute("title", "运行中，可能还会启动更多 agent");
    expect(screen.getByTestId("workflow-rail")).toHaveAttribute("data-total", "provisional");

    rerender(<MemoryRouter initialEntries={["/s/inst_test"]}>{element("completed")}</MemoryRouter>);
    expect(countEl().textContent).toContain("4/4");
    expect(countEl().textContent).not.toContain("+");
    expect(countEl().getAttribute("title")).toBeNull();
    expect(screen.getByTestId("workflow-rail")).toHaveAttribute("data-total", "known");
  });

  it("keeps the header elapsed ticking off totals.elapsedMs when launchedAt is absent", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-18T13:00:00.000Z"));
    try {
      renderCard(
        <WorkflowTimelineCard
          run={run({ totals: elapsedTotals(u(60_000)) })}
          phases={[phase()]}
          members={[member({ memberId: "a", label: known("review:security"), state: "running" })]}
        />,
      );
      const head = screen.getByTestId("workflow-card-head");
      expect(head.textContent).toContain("1m 00s");
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      expect(head.textContent).toContain("1m 02s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("never under-counts header elapsed on an attached run with a late launchedAt", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-18T13:00:00.000Z"));
    try {
      renderCard(
        <WorkflowTimelineCard
          run={run({ totals: elapsedTotals(u(60_000)), launchedAt: "2026-09-18T12:59:55.000Z" })}
          phases={[phase()]}
          members={[member({ memberId: "a", label: known("review:security"), state: "running" })]}
        />,
      );
      const head = screen.getByTestId("workflow-card-head");
      // launchedAt would claim 5 s; the producer snapshot says 60 s — show 60.
      expect(head.textContent).toContain("1m 00s");
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      // Extrapolated snapshot (62) beats the launch clock (7).
      expect(head.textContent).toContain("1m 02s");
    } finally {
      vi.useRealTimers();
    }
  });
});
