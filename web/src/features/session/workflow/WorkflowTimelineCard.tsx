/**
 * Workflow timeline card (workbench batch W).
 *
 * Hangs directly on the Workflow tool row — no outer frame. One structure for
 * the running and finished card: the header doubles as the collapsed summary.
 * While the run is alive it stays expanded and live; on a terminal state it
 * auto-collapses to the header one-liner and expands on click.
 *
 * All rules (states, fold, grid, narrow omissions, degraded fallback) live in
 * the pure projection; this file only renders and owns toggle state.
 */
import { Fragment, useEffect, useId, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../../types/generated";
import type { SubagentRef } from "../assemble";
import { ToolCard } from "../ToolCard";
import { isToolFailure } from "../assemble";
import sessionCss from "../session.module.css";
import { subagentHref } from "../subagent/SubagentRows";
import {
  fmtDuration,
  fmtTokens,
  layoutRows,
  projectWorkflow,
  type WfAgent,
  type WfCard,
  type WfPhaseView,
  type WfState,
  type WfStatus,
} from "./workflowProgress";
import css from "./workflow.module.css";

const STATUS_WORD: Record<WfStatus, string> = {
  running: "运行中",
  completed: "已完成",
  failed: "已失败",
  killed: "已终止",
  paused: "暂停",
};

const STATE_WORD: Record<WfState, string> = {
  queued: "排队中",
  running: "运行中",
  done: "已完成",
  failed: "已失败",
  killed: "已终止",
};

/** Strip the vendor prefix so the chip reads `opus-5`, not `claude-opus-5`. */
function shortModel(model?: string): string | undefined {
  if (!model) return undefined;
  return model.replace(/^claude-/i, "");
}

/** 「还有 4 个已完成」 / 「还有 4 个（2 已完成 / 2 排队中）」. */
function foldedLabel(agents: WfAgent[], expanded: boolean): string {
  const n = agents.length;
  const done = agents.filter((a) => a.state === "done").length;
  const queued = agents.filter((a) => a.state === "queued").length;
  const killed = agents.filter((a) => a.state === "killed").length;
  const verb = expanded ? "收起" : "还有";
  if (n === done) return `… ${verb} ${n} 个已完成`;
  const parts: string[] = [];
  if (done) parts.push(`${done} 已完成`);
  if (queued) parts.push(`${queued} 排队中`);
  if (killed) parts.push(`${killed} 已终止`);
  return `… ${verb} ${n} 个（${parts.join(" / ")}）`;
}

function Chevron({ className = "" }: { className?: string }) {
  return (
    <svg className={`${css.chev} ${className}`} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <path d="m9 18 6-6-6-6" />
    </svg>
  );
}

function WorkflowGlyph() {
  return (
    <svg className={css.ico} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <rect width="8" height="8" x="3" y="3" rx="2" />
      <path d="M7 11v4a2 2 0 0 0 2 2h4" />
      <rect width="8" height="8" x="13" y="13" rx="2" />
    </svg>
  );
}

function StateGlyph({ state }: { state: WfState }) {
  const cls = `${css.stateIco} ${
    state === "done"
      ? css.stateIcoDone
      : state === "failed"
        ? css.stateIcoFailed
        : state === "killed"
          ? css.stateIcoKilled
          : ""
  }`;
  if (state === "running") {
    return (
      <svg className={`${cls} ${css.spin}`} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
        <circle cx="12" cy="12" r="9" strokeOpacity="0.24" />
        <path d="M21 12a9 9 0 1 1-6.219-8.56" />
      </svg>
    );
  }
  if (state === "done") {
    return (
      <svg className={cls} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="12" cy="12" r="9" />
        <path d="m8.5 12 2.5 2.5 4.5-4.5" />
      </svg>
    );
  }
  if (state === "failed") {
    return (
      <svg className={cls} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="12" cy="12" r="9" />
        <path d="m14.5 9.5-5 5" />
        <path d="m9.5 9.5 5 5" />
      </svg>
    );
  }
  if (state === "killed") {
    return (
      <svg className={cls} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
        <circle cx="12" cy="12" r="9" />
        <path d="m5.6 5.6 12.8 12.8" />
      </svg>
    );
  }
  return (
    <svg className={cls} aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7v5h3.5" />
    </svg>
  );
}

function Chip({ status }: { status: WfStatus }) {
  return (
    <span className={css.chip}>
      {status === "running" ? <StateGlyph state="running" /> : <span className={css.chipDot} aria-hidden="true" />}
      {STATUS_WORD[status]}
    </span>
  );
}

/** Inline fold of a member's live tool calls. */
function MemberToolFold({ subagent }: { subagent: SubagentRef }) {
  const [open, setOpen] = useState(false);
  if (!subagent.nodes.length) return null;
  return (
    <div className={css.memberTools}>
      <button
        type="button"
        className={css.memberToolsToggle}
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        {open ? "▾" : "▸"} {subagent.nodes.length} tool calls
      </button>
      {open ? (
        <div className={css.memberToolRows}>
          {subagent.nodes.map((node) =>
            isToolFailure(node) ? (
              <div key={node.id} className={css.memberToolFail} data-tool-outcome={node.result?.outcome}>
                <ToolCard
                  driverKind={node.driverKind}
                  call={node.call}
                  result={node.result}
                  completeness={node.completeness}
                  diffState={node.diffState}
                />
              </div>
            ) : (
              <ToolCard
                key={node.id}
                driverKind={node.driverKind}
                call={node.call}
                result={node.result}
                completeness={node.completeness}
                diffState={node.diffState}
              />
            ),
          )}
        </div>
      ) : null}
    </div>
  );
}

function AgentRow({
  agent,
  rowId,
  memberRef,
  starting,
}: {
  agent: WfAgent;
  rowId?: string;
  memberRef?: SubagentRef;
  starting: boolean;
}) {
  const model = shortModel(agent.model);
  const { instanceId = "" } = useParams();
  return (
    <li
      className={css.agent}
      data-state={agent.state}
      data-testid="workflow-agent"
      data-agent-id={agent.id}
      id={rowId}
    >
      <span className={css.state}>
        <StateGlyph state={agent.state} />
        <span className={css.sr}>{STATE_WORD[agent.state]}</span>
      </span>
      <Link
        className={css.agentOpen}
        to={subagentHref(instanceId, agent.id)}
        data-testid="workflow-agent-open"
        title="打开子会话"
      >
        <span className={css.agentLabel} title={agent.label}>
          <span className={css.agentName}>{agent.label}</span>
          {agent.attempt && agent.attempt > 1 ? (
            <span className={css.retry} title={`第 ${agent.attempt} 次尝试`}>
              ×{agent.attempt}
            </span>
          ) : null}
        </span>
        <span className={css.agentMeta}>
          {model ? <span className={`${css.model} ${css.soft}`}>{model}</span> : null}
          {starting ? <span className={css.soft}>启动中</span> : null}
          {!starting && agent.latestTool ? <span className={`${css.tool} ${css.soft}`}>{agent.latestTool}</span> : null}
          {!starting && typeof agent.durationMs === "number" && agent.durationMs > 0 ? <b>{fmtDuration(agent.durationMs)}</b> : null}
          {!starting && typeof agent.tokens === "number" && agent.tokens > 0 ? <b>{fmtTokens(agent.tokens)}</b> : null}
        </span>
      </Link>
      <span className={css.agentOpenBtn}>
        <Link
          className={sessionCss.openBtn}
          to={subagentHref(instanceId, agent.id)}
          data-testid="workflow-agent-open-btn"
        >
          打开
        </Link>
      </span>
      {memberRef ? <MemberToolFold subagent={memberRef} /> : null}
    </li>
  );
}

function FoldToggle({
  phaseId,
  folded,
  open,
  onToggle,
}: {
  phaseId: string;
  folded: WfAgent[];
  open: boolean;
  onToggle: () => void;
}) {
  return (
    <li className={css.more}>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={folded.map((a) => `fold-${phaseId}-${a.id}`).join(" ")}
        onClick={onToggle}
      >
        {foldedLabel(folded, open)}
      </button>
    </li>
  );
}

function PhaseBlock({ phase, refsByAgent }: { phase: WfPhaseView; refsByAgent: Map<string, SubagentRef> }) {
  // Follow `expandedByDefault` until the user toggles, then remember it.
  // Following the prop matters because the phase head mounts before members
  // stream in: an initializer would lock it to "0 agents → collapsed" forever.
  const [toggled, setToggled] = useState<boolean | null>(null);
  const open = toggled ?? phase.expandedByDefault;
  const [showFolded, setShowFolded] = useState(false);
  const bodyId = useMemo(() => `wf-phase-${phase.id.replace(/[^a-zA-Z0-9_-]/g, "_")}`, [phase.id]);
  const laid = useMemo(() => layoutRows(phase.agents), [phase]);

  const onKey = (event: React.KeyboardEvent) => {
    // Only a button the user is focused on can collapse; keys sent to an
    // attached terminal never reach this handler (lib/keyboardScope guard at
    // the app boundary keeps Escape/Tab in the xterm).
    if (event.key === "Escape" && open) {
      event.stopPropagation();
      setToggled(false);
    }
  };

  return (
    <div className={css.phase} data-testid="workflow-phase" data-grid={phase.grid ? "1" : "0"}>
      <button
        type="button"
        className={css.phaseHead}
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={() => setToggled(!open)}
        onKeyDown={onKey}
      >
        <Chevron />
        <span className={css.phaseTitle}>{phase.title}</span>
        <span className={css.phaseCount}>{phase.countText}</span>
        <span className={css.phaseMeta}>{phase.metaText}</span>
      </button>
      {open ? (
        <ul className={`${css.agents} ${phase.grid ? css.agentsGrid : ""}`} id={bodyId}>
          {laid.rows.map((agent, index) => (
            <Fragment key={agent.id}>
              {laid.foldIndex === index ? <FoldToggle key="fold" phaseId={phase.id} folded={laid.folded} open={showFolded} onToggle={() => setShowFolded(!showFolded)} /> : null}
              <AgentRow
                agent={agent}
                memberRef={refsByAgent.get(agent.id)}
                starting={agent.state === "queued" && !refsByAgent.has(agent.id) && (agent.calls ?? 0) === 0}
              />
            </Fragment>
          ))}
          {laid.foldIndex === laid.rows.length ? (
            <FoldToggle key="fold-end" phaseId={phase.id} folded={laid.folded} open={showFolded} onToggle={() => setShowFolded(!showFolded)} />
          ) : null}
          {showFolded
            ? laid.folded.map((agent) => (
                <AgentRow
                  key={`fold-row-${phase.id}-${agent.id}`}
                  rowId={`fold-${phase.id}-${agent.id}`}
                  agent={agent}
                  memberRef={refsByAgent.get(agent.id)}
                  starting={agent.state === "queued" && !refsByAgent.has(agent.id) && (agent.calls ?? 0) === 0}
                />
              ))
            : null}
        </ul>
      ) : null}
    </div>
  );
}

/** 1s wall-clock driver for a running expanded card. */
function useElapsed(active: boolean, snapshotMs: number): number {
  const [elapsed, setElapsed] = useState(snapshotMs);
  // A new snapshot is authoritative: snap to it.
  useEffect(() => setElapsed(snapshotMs), [snapshotMs]);
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => setElapsed((ms) => ms + 1000), 1000);
    return () => window.clearInterval(timer);
  }, [active, snapshotMs]);
  return elapsed;
}

function DetailedCard({ card, refsByAgent }: { card: WfCard; refsByAgent: Map<string, SubagentRef> }) {
  const running = card.status === "running";
  const [open, setOpen] = useState(running);
  const rawId = useId();
  const bodyId = `wf-body-${rawId.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
  // Auto-collapse when the run reaches a terminal state (once).
  const [wasRunning, setWasRunning] = useState(running);
  useEffect(() => {
    if (wasRunning && !running) {
      setOpen(false);
      setWasRunning(false);
    }
  }, [running, wasRunning]);
  // Wall clock while the expanded card is live, snapping to each snapshot.
  const elapsedMs = useElapsed(running && open, card.totals.elapsedMs);

  const agentsWord = card.totals.totalKnown
    ? `${card.totals.done + card.totals.failed + card.totals.killed}/${card.totals.agentsTotal} agents`
    : `${card.totals.done + card.totals.failed + card.totals.killed} agents`;

  const onKey = (event: React.KeyboardEvent) => {
    if (event.key === "Escape" && open) {
      event.stopPropagation();
      setOpen(false);
    }
  };

  return (
    <div className={css.card} data-status={card.status} data-x-ui="workflow-card" data-testid="workflow-card">
      <button
        type="button"
        className={css.head}
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={() => setOpen(!open)}
        onKeyDown={onKey}
      >
        <Chevron />
        <WorkflowGlyph />
        <span className={css.name}>
          <em>Workflow</em>
          {card.name}
        </span>
        <Chip status={card.status} />
        <span className={css.meta}>
          <span className={css.rail} aria-hidden="true">
            <span className={css.railFill} style={{ width: `${card.railPct}%` }} />
          </span>
          <span>{agentsWord}</span>
          {elapsedMs > 0 ? <span>{fmtDuration(elapsedMs)}</span> : null}
          {card.totals.tokens > 0 ? (
            <span>
              <i>{fmtTokens(card.totals.tokens)}</i> tokens
            </span>
          ) : null}
          {card.totals.calls > 0 ? (
            <span>
              <i>{card.totals.calls}</i> 次调用
            </span>
          ) : null}
        </span>
      </button>
      {open ? (
        <div id={bodyId}>
          {running && card.live ? (
            <p className={css.live}>
              <b>当前</b>
              <span>
                {card.live.phase}: {card.live.agent}
              </span>
            </p>
          ) : null}
          {!running && card.summary ? (
            <p className={css.live}>
              <b>结果</b>
              <span>{card.summary}</span>
            </p>
          ) : null}
          <div className={css.body}>
            {card.phases.map((phase) => (
              <PhaseBlock key={phase.id} phase={phase} refsByAgent={refsByAgent} />
            ))}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function FlatCard({ card }: { card: WfCard }) {
  return (
    <div className={css.card} data-status={card.status} data-flat="1" data-x-ui="workflow-card" data-testid="workflow-card-flat">
      <div className={css.head} role="group" aria-label={`Workflow ${card.name} ${STATUS_WORD[card.status]}`}>
        <Chevron />
        <WorkflowGlyph />
        <span className={css.name}>
          <em>Workflow</em>
          {card.name}
        </span>
        <Chip status={card.status} />
        <span className={css.meta}>
          {card.totals.elapsedMs > 0 ? <span>{fmtDuration(card.totals.elapsedMs)}</span> : null}
          {card.note ? <span className={css.note}>{card.note}</span> : null}
        </span>
      </div>
    </div>
  );
}

export function WorkflowTimelineCard(props: {
  run: WorkflowRunPayload;
  phases: WorkflowPhasePayload[];
  members: WorkflowMemberPayload[];
  /** c-wfdrill: live tool rows folded per member, keyed by native agent id. */
  subagents?: SubagentRef[];
}) {
  const card = useMemo(
    () => projectWorkflow({ run: props.run, phases: props.phases, members: props.members }),
    [props.run, props.phases, props.members],
  );
  const refsByAgent = useMemo(() => {
    const map = new Map<string, SubagentRef>();
    for (const ref of props.subagents ?? []) map.set(ref.agentId, ref);
    return map;
  }, [props.subagents]);
  return card.detailed ? (
    <DetailedCard card={card} refsByAgent={refsByAgent} />
  ) : (
    <FlatCard card={card} />
  );
}
