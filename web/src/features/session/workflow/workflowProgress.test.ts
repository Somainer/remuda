import { describe, expect, it } from "vitest";
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../../types/generated";
import {
  FOLD_THRESHOLD,
  GRID_THRESHOLD,
  degradedCard,
  foldAgents,
  fmtDuration,
  fmtTokens,
  layoutRows,
  memberState,
  projectWorkflow,
  runStatus,
  type WfAgent,
  type WfState,
} from "./workflowProgress";

const known = <T,>(value: T) => ({ state: "known" as const, value });
const unknown = { state: "unknown" as const, reason: "not-emitted", evidenceEventIds: [] };
const u = (n: number | string) => String(n);

function agent(partial: Partial<WfAgent> & { id: string }): WfAgent {
  return { state: "queued" as WfState, ...partial };
}

function member(partial: Partial<WorkflowMemberPayload> & { memberId: string; phaseId?: string | null }): WorkflowMemberPayload {
  return {
    workflowId: "wf_demo",
    memberId: partial.memberId,
    nativeAgentId: partial.nativeAgentId ?? known(`agent-${partial.memberId}`),
    nativeKey: partial.nativeKey ?? known(`key-${partial.memberId}`),
    attempt: partial.attempt ?? unknown,
    phaseId: partial.phaseId ?? "p1",
    label: partial.label ?? known(`label-${partial.memberId}`),
    state: partial.state ?? "running",
    modelRequested: partial.modelRequested ?? unknown,
    modelResolved: partial.modelResolved ?? known("opus-5[1m]"),
    resultRef: null,
    revision: u(1),
    latestTool: partial.latestTool ?? null,
    tokens: partial.tokens ?? null,
    calls: partial.calls ?? null,
    durationMs: partial.durationMs ?? null,
    startedAt: null,
    endedAt: null,
  };
}

function phase(partial: Partial<WorkflowPhasePayload> & { phaseId: string }): WorkflowPhasePayload {
  return {
    workflowId: "wf_demo",
    phaseId: partial.phaseId,
    nativePhaseId: partial.nativePhaseId ?? known(partial.phaseId),
    label: partial.label ?? known(partial.phaseId === "p1" ? "Review" : "Verify"),
    state: partial.state ?? "running",
    revision: u(1),
    parentPhaseId: null,
  };
}

function run(partial: Partial<WorkflowRunPayload> = {}): WorkflowRunPayload {
  return {
    workflowId: "wf_demo",
    engine: "claude-workflow",
    nativeRunId: known("wf_demo_native"),
    nativeTaskId: known("task-1"),
    toolCallId: "call-1",
    state: partial.state ?? "running",
    revision: u(partial.revision ?? 1),
    title: partial.title ?? known("demo run"),
    name: partial.name ?? known("demo-wf"),
    description: partial.description ?? known("demo workflow"),
    totals: partial.totals ?? null,
    live: partial.live ?? null,
    note: partial.note ?? null,
    resultRef: null,
  };
}

describe("fmtDuration", () => {
  it("formats minutes and zero-padded seconds", () => {
    expect(fmtDuration(222_000)).toBe("3m 42s");
    expect(fmtDuration(41_000)).toBe("0m 41s");
  });
  it("renders an em dash for missing/zero durations", () => {
    expect(fmtDuration(0)).toBe("—");
    expect(fmtDuration(undefined)).toBe("—");
  });
});

describe("fmtTokens", () => {
  it("compacts with k/M and no trailing .0", () => {
    expect(fmtTokens(9_100)).toBe("9.1k");
    expect(fmtTokens(312_000)).toBe("312k");
    expect(fmtTokens(2_400_000)).toBe("2.4M");
    expect(fmtTokens(48_000)).toBe("48k");
  });
});

describe("state wording", () => {
  it("maps protocol states to card states", () => {
    expect(memberState("completed")).toBe("done");
    expect(memberState("failed")).toBe("failed");
    expect(memberState("cancelled")).toBe("killed");
    expect(memberState("queued")).toBe("queued");
    expect(memberState("running")).toBe("running");
  });
  it("maps run states; cancelled reads killed (已终止)", () => {
    expect(runStatus("completed")).toBe("completed");
    expect(runStatus("failed")).toBe("failed");
    expect(runStatus("cancelled")).toBe("killed");
    expect(runStatus("running")).toBe("running");
  });
});

describe("foldAgents", () => {
  it("keeps all agents under the threshold", () => {
    const agents = Array.from({ length: 10 }, (_, i) => agent({ id: `a${i}`, state: "done" }));
    const { pinned, head, folded } = foldAgents(agents);
    expect(pinned).toHaveLength(0);
    expect(head).toHaveLength(10);
    expect(folded).toHaveLength(0);
  });

  it("folds only trailing quiet rows past the threshold", () => {
    const agents: WfAgent[] = [];
    // 14 done, then a running and a failed after them.
    for (let i = 0; i < 14; i++) agents.push(agent({ id: `d${i}`, state: "done" }));
    agents.push(agent({ id: "run", state: "running" }));
    agents.push(agent({ id: "fail", state: "failed" }));
    const { pinned, head, folded } = foldAgents(agents);
    expect(pinned.map((a) => a.id)).toEqual(["run", "fail"]);
    expect(head).toHaveLength(FOLD_THRESHOLD - 2);
    expect(folded).toHaveLength(14 - (FOLD_THRESHOLD - 2));
  });

  it("never folds running or failed rows even past 12 pinned", () => {
    const agents: WfAgent[] = [];
    for (let i = 0; i < 14; i++) agents.push(agent({ id: `r${i}`, state: "running" }));
    for (let i = 0; i < 10; i++) agents.push(agent({ id: `d${i}`, state: "done" }));
    const { pinned, folded } = foldAgents(agents);
    expect(pinned).toHaveLength(14);
    expect(folded).toHaveLength(10);
  });
});

describe("layoutRows", () => {
  it("places the fold marker in the tail's index position", () => {
    const agents: WfAgent[] = [];
    for (let i = 0; i < 12; i++) agents.push(agent({ id: `q${i}`, state: "done" }));
    agents.push(agent({ id: "run", state: "running" }));
    const { rows, foldIndex, folded } = layoutRows(agents);
    expect(folded).toHaveLength(1);
    expect(foldIndex).not.toBeNull();
    // 11 quiet rows fit beside the pinned running row (12 visible); fold marker
    // sits at row 11, before run.
    expect(rows).toHaveLength(12);
    expect(rows.at(-1)!.id).toBe("run");
  });
});

describe("grid threshold", () => {
  it("switches a phase to grid layout only above 8 agents", () => {
    const eight = projectWorkflow({
      run: run(),
      phases: [phase({ phaseId: "p1" })],
      members: Array.from({ length: 8 }, (_, i) => member({ memberId: `m${i}`, state: "running" })),
    });
    expect(eight.phases[0].grid).toBe(false);
    const nine = projectWorkflow({
      run: run(),
      phases: [phase({ phaseId: "p1" })],
      members: Array.from({ length: 9 }, (_, i) => member({ memberId: `m${i}`, state: "running" })),
    });
    expect(nine.phases[0].grid).toBe(true);
    expect(GRID_THRESHOLD).toBe(8);
  });
});

describe("phase meta wording", () => {
  it("says 全部排队中 when nothing started", () => {
    const card = projectWorkflow({
      run: run(),
      phases: [phase({ phaseId: "p1", state: "queued" })],
      members: [
        member({ memberId: "a", state: "queued" }),
        member({ memberId: "b", state: "queued" }),
      ],
    });
    expect(card.phases[0].metaText).toBe("全部排队中");
    expect(card.phases[0].countText).toBe("0/2");
  });

  it("counts running agents and shows duration", () => {
    const card = projectWorkflow({
      run: run(),
      phases: [phase({ phaseId: "p1" })],
      members: [
        member({ memberId: "a", state: "completed", durationMs: u(222_000) }),
        member({ memberId: "b", state: "running", durationMs: u(62_000) }),
      ],
    });
    expect(card.phases[0].metaText).toContain("1 运行中");
    expect(card.phases[0].metaText).toContain("3m 42s");
    expect(card.phases[0].countText).toBe("1/2");
  });

  it("marks a fully done phase n/n 完成", () => {
    const card = projectWorkflow({
      run: run({ state: "completed" }),
      phases: [phase({ phaseId: "p1", state: "completed" })],
      members: [member({ memberId: "a", state: "completed" }), member({ memberId: "b", state: "completed" })],
    });
    expect(card.phases[0].countText).toBe("2/2 完成");
  });

  it("reports killed agents with 已终止 in phase meta", () => {
    const card = projectWorkflow({
      run: run({ state: "cancelled" }),
      phases: [phase({ phaseId: "p1", state: "cancelled" })],
      members: [
        member({ memberId: "a", state: "cancelled" }),
        member({ memberId: "b", state: "cancelled" }),
        member({ memberId: "c", state: "queued" }),
        member({ memberId: "d", state: "queued" }),
      ],
    });
    expect(card.phases[0].metaText).toContain("2 已终止");
    expect(card.phases[0].metaText).toContain("2 排队中");
    expect(card.status).toBe("killed");
  });
});

describe("20-agent phase fold", () => {
  it("folds the quiet tail and reports 还有 n in the view model", () => {
    const members = Array.from({ length: 20 }, (_, i) =>
      member({ memberId: `m${String(i).padStart(2, "0")}`, state: "completed" }),
    );
    const card = projectWorkflow({
      run: run({ state: "completed" }),
      phases: [phase({ phaseId: "p1", state: "completed" })],
      members,
    });
    const p1 = card.phases[0];
    expect(p1.canFold).toBe(true);
    expect(p1.folded).toHaveLength(20 - FOLD_THRESHOLD);
    const laid = layoutRows(
      members.map((m, i) => agent({ id: `m${String(i).padStart(2, "0")}`, state: "done" })),
    );
    expect(laid.folded).toHaveLength(20 - FOLD_THRESHOLD);
  });
});

describe("failed rows never folded", () => {
  it("keeps a late failed row visible and folds done rows", () => {
    const agents: WfAgent[] = Array.from({ length: 14 }, (_, i) => agent({ id: `d${i}`, state: "done" }));
    agents.push(agent({ id: "boom", state: "failed" }));
    const { rows, folded } = layoutRows(agents);
    expect(folded).toHaveLength(3);
    expect(rows.map((a) => a.id)).toContain("boom");
  });
});

describe("card totals and rail", () => {
  it("derives totals from members when the run omits the totals block", () => {
    const card = projectWorkflow({
      run: run({ state: "running" }),
      phases: [phase({ phaseId: "p1" }), phase({ phaseId: "p2", label: known("Verify") })],
      members: [
        member({ memberId: "a", phaseId: "p1", state: "completed", tokens: u(48_000), calls: u(3) }),
        member({ memberId: "b", phaseId: "p1", state: "running", tokens: u(21_000), calls: u(2) }),
        member({ memberId: "c", phaseId: "p2", state: "queued" }),
      ],
    });
    expect(card.totals.done).toBe(1);
    expect(card.totals.running).toBe(1);
    expect(card.totals.queued).toBe(1);
    expect(card.totals.tokens).toBe(69_000);
    expect(card.totals.calls).toBe(5);
    expect(card.railPct).toBe(33);
  });

  it("uses payload totals when present and trusts totalKnown=false", () => {
    const card = projectWorkflow({
      run: run({
        state: "running",
        totals: {
          totalKnown: false,
          agentsTotal: u(4),
          agentsDone: u(2),
          agentsFailed: u(0),
          agentsKilled: u(0),
          agentsRunning: u(2),
          tokens: u(1000),
          calls: u(9),
          elapsedMs: u(1000),
        },
      }),
      phases: [phase({ phaseId: "p1" })],
      members: [
        member({ memberId: "a", state: "completed" }),
        member({ memberId: "b", state: "completed" }),
        member({ memberId: "c", state: "running" }),
        member({ memberId: "d", state: "running" }),
      ],
    });
    expect(card.totals.totalKnown).toBe(false);
    expect(card.railPct).toBe(0);
  });
});

describe("attempt marker", () => {
  it("passes attempt through for the ×n marker", () => {
    const card = projectWorkflow({
      run: run({ state: "completed" }),
      phases: [phase({ phaseId: "p1", state: "completed" })],
      members: [member({ memberId: "a", state: "completed", attempt: known(u(2)) })],
    });
    expect(card.phases[0].head[0].attempt).toBe(2);
  });
});

describe("degraded card", () => {
  it("is a flat row carrying only the explanation (decision 6)", () => {
    const card = degradedCard("legacy-wf", "running", "daemon 版本较旧，暂无阶段明细");
    expect(card.detailed).toBe(false);
    expect(card.phases).toHaveLength(0);
    expect(card.note).toContain("阶段明细");
  });
});
