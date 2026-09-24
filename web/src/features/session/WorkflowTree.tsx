import { Link, useParams } from "react-router-dom";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload } from "../../types/generated";
import { knowledgeValue } from "../../types/command";
import { subagentHref } from "./subagent/SubagentRows";
import css from "./toolCard.module.css";

export function WorkflowTree({
  run,
  phases,
  members,
}: {
  run: WorkflowRunPayload;
  phases: WorkflowPhasePayload[];
  members: WorkflowMemberPayload[];
}) {
  const open = run.state === "running" || run.state === "failed" || run.state === "unknown";
  const running = run.state === "running";
  return (
    <details className={css.tool} open={open} data-testid="workflow-tree">
      <summary className={css.toolHead}>
        <span className={css.toolTitle}>Workflow</span>
        <span className={css.path}>{knowledgeValue(run.nativeRunId) ?? run.workflowId}</span>
        <span className={css.toolStatus}>
          {running ? <span className={css.runDot} /> : <span className={css.okDot} />}
          {memberStateText(run.state)}
        </span>
        <span className={css.spacer} />
        <span className={css.stat}>只画身份与状态，log 进原始事件</span>
      </summary>
      <div className={css.wfMembers}>
        {phases.map((phase) => {
          const phaseMembers = members.filter((m) => m.phaseId === phase.phaseId);
          return (
            <div key={phase.phaseId} data-testid="workflow-phase">
              <div className={css.wfPhase}>
                <span>▾</span>
                <span>{knowledgeValue(phase.label) ?? phase.phaseId}</span>
              </div>
              <ul className={css.wfMembers}>
                {phaseMembers.map((m) => (
                  <MemberRow key={m.memberId} member={m} />
                ))}
              </ul>
            </div>
          );
        })}
        {unphasedMembers(phases, members).length ? (
          <ul className={css.wfMembers}>
            {unphasedMembers(phases, members).map((m) => (
              <MemberRow key={m.memberId} member={m} />
            ))}
          </ul>
        ) : null}
      </div>
    </details>
  );
}

function unphasedMembers(phases: WorkflowPhasePayload[], members: WorkflowMemberPayload[]): WorkflowMemberPayload[] {
  return members.filter((m) => !m.phaseId || !phases.some((p) => p.phaseId === m.phaseId));
}

function memberStateText(state: WorkflowMemberPayload["state"]): string {
  return state;
}

function MemberRow({ member }: { member: WorkflowMemberPayload }) {
  const { instanceId = "" } = useParams();
  // Workflow members are Claude sub-sessions inside THIS session, never
  // Remuda instances: drill into the agent route keyed by the native agent
  // id instead of linking a (never-set) child instance.
  const agentId = knowledgeValue(member.nativeAgentId) ?? member.memberId;
  const starting = member.state === "queued";
  const model = knowledgeValue(member.modelResolved) ?? knowledgeValue(member.modelRequested);
  const label = knowledgeValue(member.label) ?? member.memberId;
  return (
    <li className={css.member} data-testid="workflow-member" data-state={member.state}>
      <span className={runningDot(member.state)} />
      <span className={css.memberName}>{label}</span>
      <span className={css.memberMeta}>
        {member.state}
        {starting ? " · 启动中" : ""}
        {model ? ` · ${model}` : ""}
      </span>
      <Link className={css.openBtn} to={subagentHref(instanceId, agentId)} data-testid="workflow-member-open">
        打开
      </Link>
    </li>
  );
}

function runningDot(state: WorkflowMemberPayload["state"]): string {
  return state === "running" ? css.runDot : css.okDot;
}
