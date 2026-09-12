import { useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Button } from "../components/Button";
import { StateDot } from "../components/StateDot";
import { hubStore, useHub } from "../lib/store";
import { projectStatus } from "../lib/status";
import ui from "../styles/ui.module.css";

export function ApprovalsPage() {
  const hub = useHub();
  const [params] = useSearchParams();
  const focus = params.get("focus");
  const [filter, setFilter] = useState<"all" | "approval" | "question" | "plan-review">("all");
  const [busyId, setBusyId] = useState<string | null>(null);
  const items = hub.interactions.filter((i) => (filter === "all" ? true : i.kind === filter));

  return (
    <div style={{ padding: 16, maxWidth: 720 }}>
      <h1 style={{ fontSize: 18 }}>审批中心</h1>
      <p className={ui.listMeta}>待处理 {items.filter((i) => i.state === "pending").length} · 本设备已处理的会从队列消失</p>
      <div className={ui.row} style={{ margin: "12px 0" }}>
        {(["all", "approval", "question", "plan-review"] as const).map((id) => (
          <button key={id} className={`${ui.chip} ${filter === id ? ui.chipOn : ""}`} onClick={() => setFilter(id)}>
            {id === "all" ? "全部" : id}
          </button>
        ))}
      </div>
      {items.map((item) => {
        const instance = hub.instances.find((i) => i.id === item.instanceId);
        const focused = focus === item.id;
        return (
          <article key={item.id} className={ui.card} style={{ marginBottom: 12, outline: focused ? "1px solid var(--dust)" : undefined }}>
            <div className={ui.cardHead}>
              {instance ? <StateDot status={projectStatus(instance)} /> : null}
              <span>
                {hubStore.hostName(item.hostId)} / {hubStore.workspaceOf(instance?.workspaceId ?? "")?.label} / {instance?.kind}
              </span>
            </div>
            {item.kind === "approval" ? (
              <ApprovalCard
                interaction={item}
                busy={busyId === item.id}
                onRespond={(answer) => {
                  setBusyId(item.id);
                  void hubStore.respond(item.id, answer).finally(() => setBusyId(null));
                }}
              />
            ) : item.kind === "question" ? (
              <>
                <p>
                  问你 {item.request.kind === "question" ? item.request.fields.length : 0} 题 · AskUserQuestion
                </p>
                <div className={ui.row}>
                  <Link to={`/s/${item.instanceId}`}>
                    <Button variant="primary">去回答</Button>
                  </Link>
                </div>
                {focused ? (
                  <QuestionForm
                    interaction={item}
                    busy={busyId === item.id}
                    onRespond={(answer) => {
                      setBusyId(item.id);
                      void hubStore.respond(item.id, answer).finally(() => setBusyId(null));
                    }}
                  />
                ) : null}
              </>
            ) : (
              <p>{item.kind}</p>
            )}
            <div className={ui.row} style={{ marginTop: 8 }}>
              <Link to={`/s/${item.instanceId}`}>打开会话</Link>
            </div>
          </article>
        );
      })}
      {items.length === 0 ? <p className={ui.listMeta}>没有待处理交互</p> : null}
    </div>
  );
}
