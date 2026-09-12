import { Link } from "react-router-dom";
import type { WorkflowMemberPayload, WorkflowPhasePayload, WorkflowRunPayload } from "../../types/generated";
import { knowledgeValue } from "../../types/command";
import ui from "../../styles/ui.module.css";
import { memberChildInstanceId } from "./workflow";

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
  return (
    <details className={ui.card} open={open} data-testid="workflow-tree">
      <summary className={ui.cardHead}>
        <strong>Workflow</strong>
        <span>{knowledgeValue(run.nativeRunId) ?? run.workflowId}</span>
        <span>{knowledgeValue(run.title) ?? run.state}</span>
      </summary>
      {phases.map((phase) => (
        <div key={phase.phaseId} data-testid="workflow-phase" style={{ marginLeft: 12 }}>
          ▾ {knowledgeValue(phase.label) ?? phase.phaseId} · {phase.state}
          <ul>
            {members
              .filter((m) => m.phaseId === phase.phaseId)
              .map((m) => (
                <MemberRow key={m.memberId} member={m} />
              ))}
          </ul>
        </div>
      ))}
      {unphased.length ? (
        <ul>
          {unphased.map((m) => (
            <MemberRow key={m.memberId} member={m} />
          ))}
        </ul>
      ) : null}
    </details>
  );
}

function MemberRow({ member }: { member: WorkflowMemberPayload }) {
  const child = memberChildInstanceId(member);
  const model = knowledgeValue(member.modelResolved) ?? knowledgeValue(member.modelRequested);
  const label = knowledgeValue(member.label) ?? member.memberId;
  const body = (
    <>
      {label} · {member.state}
      {model ? ` · ${model}` : ""}
    </>
  );
  return (
    <li data-testid="workflow-member" data-child={child ? "1" : "0"}>
      {child ? <Link to={`/s/${child}`}>{body}</Link> : body}
    </li>
  );
}
