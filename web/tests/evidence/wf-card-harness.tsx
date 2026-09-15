/**
 * Evidence harness for the Workflow timeline card (workbench batch W).
 *
 * Dev-only mount used by web/tests/e2e/workflow-card-evidence.spec.ts to
 * screenshot the real component with synthetic payloads. No real models, no
 * reference-mock names; every label here is invented for this fixture.
 */
import { createRoot } from "react-dom/client";
import { StrictMode } from "react";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload } from "../../src/types/generated";
import { WorkflowTimelineCard } from "../../src/features/session/workflow/WorkflowTimelineCard";
import "../../src/styles/tokens.css";

const known = <T,>(value: T) => ({ state: "known" as const, value });
const unk = { state: "unknown" as const, reason: "not-emitted", evidenceEventIds: [] };
const u = (n: number) => String(n);

type M = Partial<WorkflowMemberPayload> & { memberId: string };

function run(p: Partial<WorkflowRunPayload> & { state: string }): WorkflowRunPayload {
  return {
    workflowId: "wf_ev",
    engine: "claude-workflow",
    nativeRunId: known("wf_ev"),
    nativeTaskId: known("task"),
    toolCallId: "call",
    state: p.state,
    revision: u(1),
    title: known(p.name ?? "card-evidence"),
    name: known(p.name ?? "card-evidence"),
    description: p.description ?? null,
    totals: p.totals ?? null,
    live: p.live ?? null,
    note: p.note ?? null,
    resultRef: null,
  };
}

function phase(id: string, label: string, state = "running"): WorkflowPhasePayload {
  return {
    workflowId: "wf_ev",
    phaseId: id,
    nativePhaseId: known(id),
    label: known(label),
    state,
    revision: u(1),
    parentPhaseId: null,
  };
}

function member(m: M, phaseId: string, state: string): WorkflowMemberPayload {
  return {
    workflowId: "wf_ev",
    memberId: m.memberId,
    nativeAgentId: known(`a-${m.memberId}`),
    nativeKey: unk,
    attempt: m.attempt ?? unk,
    phaseId,
    label: known(m.label ?? m.memberId),
    state,
    modelRequested: unk,
    modelResolved: m.modelResolved ?? known("haiku-4.5"),
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

const totals = (done: number, failed: number, killed: number, runningN: number, total: number, tokens: number, calls: number, ms: number) =>
  ({
    totalKnown: true,
    agentsTotal: u(total),
    agentsDone: u(done),
    agentsFailed: u(failed),
    agentsKilled: u(killed),
    agentsRunning: u(runningN),
    tokens: u(tokens),
    calls: u(calls),
    elapsedMs: u(ms),
  }) as const;

function Running() {
  const phases = [phase("p1", "Review"), phase("p2", "Verify")];
  const members = [
    member({ memberId: "a", label: "check:inputs", latestTool: known("Read"), tokens: u(48_000), calls: u(4), durationMs: u(130_000) }, "p1", "completed"),
    member({ memberId: "b", label: "check:outputs", latestTool: known("Grep"), tokens: u(39_000), calls: u(3), durationMs: u(108_000) }, "p1", "completed"),
    member({ memberId: "c", label: "check:security", modelResolved: known("opus-5[1m]"), latestTool: known("Grep"), tokens: u(21_000), calls: u(2), durationMs: u(62_000) }, "p1", "running"),
    member({ memberId: "d", label: "check:contracts" }, "p1", "queued"),
    member({ memberId: "e", label: "probe:auth" }, "p2", "queued"),
    member({ memberId: "f", label: "probe:api", attempt: known(u(2)) }, "p2", "queued"),
  ];
  return (
    <WorkflowTimelineCard
      run={run({
        state: "running",
        name: "check-and-probe",
        totals: totals(2, 0, 0, 1, 6, 108_000, 9, 222_000),
        live: { phaseTitle: known("Review"), agentLabel: known("check:security"), summary: unk },
      })}
      phases={phases}
      members={members}
    />
  );
}

function DoneCollapsed() {
  const phases = [phase("p1", "Review", "completed")];
  const members = [
    member({ memberId: "a", label: "check:inputs", latestTool: known("Read"), tokens: u(48_000), calls: u(4), durationMs: u(130_000) }, "p1", "completed"),
    member({ memberId: "b", label: "check:outputs", latestTool: known("Grep"), tokens: u(39_000), calls: u(3), durationMs: u(108_000) }, "p1", "completed"),
  ];
  return (
    <WorkflowTimelineCard
      run={run({
        state: "completed",
        name: "quick-check",
        totals: totals(2, 0, 0, 0, 2, 87_000, 7, 98_000),
        live: { phaseTitle: unk, agentLabel: unk, summary: known("Dynamic workflow \"quick-check\" completed") },
      })}
      phases={phases}
      members={members}
    />
  );
}

function FoldGrid() {
  const phases = [phase("p1", "Generate", "running"), phase("p2", "Validate", "queued")];
  const members: WorkflowMemberPayload[] = [];
  for (let i = 0; i < 20; i++) {
    const isRunning = i < 2;
    members.push(
      member(
        {
          memberId: `g${i}`,
          label: `gen:item-${String(i + 1).padStart(2, "0")}`,
          modelResolved: known("opus-5[1m]"),
          latestTool: isRunning ? known("Write") : null,
          tokens: isRunning ? u(7_000 + i * 100) : null,
          calls: isRunning ? u(1) : null,
          durationMs: isRunning ? u(30_000 + i * 1000) : null,
        },
        "p1",
        isRunning ? "running" : "queued",
      ),
    );
  }
  for (let i = 0; i < 16; i++) {
    members.push(member({ memberId: `v${i}`, label: `validate:item-${String(i + 1).padStart(2, "0")}` }, "p2", "queued"));
  }
  return (
    <WorkflowTimelineCard
      run={run({
        state: "running",
        name: "bulk-gen",
        totals: totals(0, 0, 0, 2, 36, 15_000, 2, 64_000),
        live: { phaseTitle: known("Generate"), agentLabel: known("gen:item-01"), summary: unk },
      })}
      phases={phases}
      members={members}
    />
  );
}

function Failed() {
  const phases = [phase("p1", "Review", "failed")];
  const members: WorkflowMemberPayload[] = [];
  for (let i = 0; i < 14; i++) {
    members.push(
      member(
        { memberId: `d${i}`, label: `check:item-${String(i).padStart(2, "0")}`, latestTool: known("Read"), tokens: u(36_000), calls: u(3), durationMs: u(90_000) },
        "p1",
        "completed",
      ),
    );
  }
  members.push(
    member(
      { memberId: "boom", label: "check:security", attempt: known(u(2)), modelResolved: known("opus-5[1m]"), latestTool: known("Bash"), tokens: u(22_000), calls: u(2), durationMs: u(72_000) },
      "p1",
      "failed",
    ),
  );
  return (
    <WorkflowTimelineCard
      run={run({
        state: "failed",
        name: "check-and-probe",
        totals: totals(14, 1, 0, 0, 15, 388_000, 94, 361_000),
        live: { phaseTitle: unk, agentLabel: unk, summary: known("Dynamic workflow \"check-and-probe\" failed") },
      })}
      phases={phases}
      members={members}
    />
  );
}

function Flat() {
  return <WorkflowTimelineCard run={run({ state: "running", name: "legacy-run", note: "daemon 版本较旧，暂无阶段明细" })} phases={[]} members={[]} />;
}

function Section({ id, title, children }: { id: string; title: string; children: React.ReactNode }) {
  return (
    <section data-evidence={id} style={{ padding: "14px 18px", borderTop: "1px dashed var(--line)" }}>
      <h2 style={{ fontFamily: "var(--mono)", fontSize: 11, color: "var(--mute)", margin: "0 0 8px" }}>{title}</h2>
      {children}
    </section>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <main style={{ maxWidth: 1040, margin: "0 auto", background: "var(--canvas)", color: "var(--paper)", fontFamily: "var(--font)" }}>
      <Section id="running" title="运行中 · 2 phase · 当前行">
        <Running />
      </Section>
      <Section id="done" title="已完成 · 自动折叠为一行">
        <DoneCollapsed />
      </Section>
      <Section id="fold" title="36 agents · 分栏 · quiet tail 折叠">
        <FoldGrid />
      </Section>
      <Section id="failed" title="已失败 · 失败行不折叠（展开后可见）">
        <Failed />
      </Section>
      <Section id="flat" title="降级 · 普通行 + 说明">
        <Flat />
      </Section>
    </main>
  </StrictMode>,
);
