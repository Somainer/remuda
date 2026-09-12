import { useMemo } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { Button } from "../components/Button";
import { StateDot } from "../components/StateDot";
import { hubStore, useHub } from "../lib/store";
import { projectStatus } from "../lib/status";
import {
  INTERACTION_LABEL,
  projectInteraction,
  thisDeviceId,
  type InteractionUiState,
} from "../lib/interactionStatus";
import ui from "../styles/ui.module.css";
import type { Interaction, InteractionAnswer } from "../types/interaction";

const KIND_FILTERS = ["all", "approval", "question", "plan-review"] as const;

function preview(item: Interaction): string {
  if (item.request.kind === "approval") return `${item.request.title}  ${item.request.description}`;
  if (item.request.kind === "question") return `问你 ${item.request.fields.length} 题 · AskUserQuestion`;
  if (item.request.kind === "plan-review") return item.request.title;
  return item.request.title;
}

export function ApprovalsPage() {
  const hub = useHub();
  const [params, setParams] = useSearchParams();
  const focus = params.get("focus");
  const kind = (params.get("kind") as (typeof KIND_FILTERS)[number] | null) ?? "all";
  const hostFilter = params.get("host") ?? "";
  const workspaceFilter = params.get("workspace") ?? "";
  const deviceId = thisDeviceId();

  const rows = useMemo(() => {
    return hub.interactions
      .map((item) => {
        const instance = hub.instances.find((i) => i.id === item.instanceId);
        const host = hub.hosts.find((h) => h.id === item.hostId);
        const uiState = projectInteraction(item, {
          answering: Boolean(hub.answering[item.id]),
          host,
          connectivity: instance?.connectivity,
          deviceId,
        });
        return { item, instance, host, uiState };
      })
      .filter((row) => {
        if (kind !== "all" && row.item.kind !== kind) return false;
        if (hostFilter && row.item.hostId !== hostFilter) return false;
        if (workspaceFilter && row.instance?.workspaceId !== workspaceFilter) return false;
        if (row.uiState === "settled") return false;
        return true;
      });
  }, [hub, kind, hostFilter, workspaceFilter, deviceId]);

  const pendingCount = rows.filter((r) => r.uiState === "pending" || r.uiState === "answering" || r.uiState === "paused").length;

  const respond = (item: Interaction, answer: InteractionAnswer) => {
    void hubStore.respond(item.id, answer);
  };

  return (
    <div style={{ padding: 16, maxWidth: 720 }} data-testid="approvals-page">
      <h1 style={{ fontSize: 18 }}>审批中心</h1>
      <p className={ui.listMeta}>待处理 {pendingCount} · 本设备已处理的会从队列消失</p>
      <div className={ui.row} style={{ margin: "12px 0" }}>
        {KIND_FILTERS.map((id) => (
          <button
            key={id}
            className={`${ui.chip} ${kind === id ? ui.chipOn : ""}`}
            onClick={() => {
              const next = new URLSearchParams(params);
              if (id === "all") next.delete("kind");
              else next.set("kind", id);
              setParams(next);
            }}
          >
            {id === "all" ? "全部" : id === "approval" ? "审批" : id === "question" ? "提问" : "计划"}
          </button>
        ))}
      </div>
      <div className={ui.row} style={{ marginBottom: 12 }}>
        {hub.hosts.map((h) => (
          <button
            key={h.id}
            className={`${ui.chip} ${hostFilter === h.id ? ui.chipOn : ""}`}
            onClick={() => {
              const next = new URLSearchParams(params);
              if (hostFilter === h.id) next.delete("host");
              else next.set("host", h.id);
              setParams(next);
            }}
          >
            {h.label}
          </button>
        ))}
        {hub.workspaces.map((w) => (
          <button
            key={w.id}
            className={`${ui.chip} ${workspaceFilter === w.id ? ui.chipOn : ""}`}
            onClick={() => {
              const next = new URLSearchParams(params);
              if (workspaceFilter === w.id) next.delete("workspace");
              else next.set("workspace", w.id);
              setParams(next);
            }}
          >
            {w.label}
          </button>
        ))}
      </div>
      {rows.map(({ item, instance, uiState }) => {
        const focused = focus === item.id;
        const paused = uiState === "paused";
        const answering = uiState === "answering";
        const showQueue = uiState === "pending" || uiState === "answering" || uiState === "paused";
        return (
          <article
            key={item.id}
            className={ui.card}
            data-testid="approval-row"
            data-state={uiState}
            style={{ marginBottom: 12, outline: focused ? "1px solid var(--dust)" : undefined }}
          >
            <div className={ui.cardHead}>
              {instance ? <StateDot status={projectStatus(instance)} /> : null}
              <span>
                {new Date(item.createdAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} · {hubStore.hostName(item.hostId)} /{" "}
                {hubStore.workspaceOf(instance?.workspaceId ?? "")?.label} / {instance?.kind}
              </span>
              <span className={ui.pill}>{INTERACTION_LABEL[uiState as InteractionUiState]}</span>
            </div>
            <p>{preview(item)}</p>
            {uiState === "expired" ? <p className={ui.listMeta}>过期，未作用于新进程</p> : null}
            {uiState === "superseded" ? <p className={ui.listMeta}>已在其它设备处理</p> : null}
            {paused ? <p className={ui.listMeta}>主机离线，交互暂停</p> : null}
            {showQueue && item.kind === "approval" && item.request.kind === "approval" ? (
              <ApprovalCard
                interaction={item}
                busy={answering || paused}
                onRespond={(answer) => respond(item, answer)}
              />
            ) : null}
            {showQueue && item.kind === "question" ? (
              <div className={ui.row}>
                <Link to={`/s/${item.instanceId}`}>
                  <Button variant="primary" disabled={paused}>
                    去回答
                  </Button>
                </Link>
              </div>
            ) : null}
            {showQueue && item.kind === "plan-review" && item.request.kind === "plan-review" ? (
              <div className={ui.row}>
                {item.request.options.map((opt) => (
                  <Button
                    key={opt.id}
                    variant={opt.effect === "deny" ? "danger" : "primary"}
                    disabled={paused || answering}
                    onClick={() =>
                      respond(item, {
                        kind: "plan-review",
                        optionId: opt.id,
                        planRevision: item.request.kind === "plan-review" ? item.request.planRevision : "1",
                        planDigest: item.request.kind === "plan-review" ? item.request.planDigest : "",
                        feedback: null,
                      })
                    }
                  >
                    {opt.label}
                  </Button>
                ))}
              </div>
            ) : null}
            <div className={ui.row} style={{ marginTop: 8 }}>
              <Link to={`/s/${item.instanceId}`}>打开会话</Link>
            </div>
          </article>
        );
      })}
      {rows.length === 0 ? <p className={ui.listMeta}>没有待处理交互</p> : null}
    </div>
  );
}
