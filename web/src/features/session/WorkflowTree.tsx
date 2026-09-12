import { Link } from "react-router-dom";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload } from "../../types/generated";
import { knowledgeValue } from "../../types/command";
import { memberChildInstanceId } from "./workflow";
import css from "./session.module.css";

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
  const unphased = members.filter((m) => !m.phaseId || !phases.some((p) => p.phaseId === m.phaseId));
  const running = run.state === "running";
  return (
    <details className={css.tool} open={open} data-testid="workflow-tree">
      <summary className={css.toolHead}>
        <span className={css.toolTitle}>Workflow</span>
        <span className={css.path}>{knowledgeValue(run.nativeRunId) ?? run.workflowId}</span>
        <span className={css.toolStatus}>
          {running ? <span className={css.runDot} /> : <span className={css.okDot} />}
          {knowledgeValue(run.title) ?? run.state}
          {running ? " · 默认展开" : ""}
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
                <span className={css.stat}>· {phaseMembers.length} members</span>
              </div>
              <ul className={css.wfMembers}>
                {phaseMembers.map((m) => (
                  <MemberRow key={m.memberId} member={m} />
                ))}
              </ul>
            </div>
          );
        })}
        {unphased.length ? (
          <ul className={css.wfMembers}>
            {unphased.map((m) => (
              <MemberRow key={m.memberId} member={m} />
            ))}
          </ul>
        ) : null}
      </div>
    </details>
  );
}

function MemberRow({ member }: { member: WorkflowMemberPayload }) {
  const child = memberChildInstanceId(member);
  const model = knowledgeValue(member.modelResolved) ?? knowledgeValue(member.modelRequested);
  const label = knowledgeValue(member.label) ?? member.memberId;
  const running = member.state === "running";
  return (
    <li className={css.member} data-testid="workflow-member" data-child={child ? "1" : "0"}>
      <span className={running ? css.runDot : css.okDot} />
      <span className={css.memberName}>{label}</span>
      <span className={css.memberMeta}>
        {member.state}
        {child ? ` · childInstanceId=${child}` : " · 无 childInstanceId，不可点进"}
        {model ? ` · ${model}` : ""}
      </span>
      {child ? (
        <Link className={css.openBtn} to={`/s/${child}`}>
          打开
        </Link>
      ) : (
        <span className={css.openOff}>打开</span>
      )}
    </li>
  );
}
