import { useMemo } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { StateDot } from "../components/StateDot";
import { PtyQuestionAnswers } from "../components/PtyQuestionAnswers";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { ElicitationCard } from "../features/approvals/ElicitationCard";
import { formatClock } from "../lib/format";
import { hubStore, useHub } from "../lib/store";
import { projectStatus } from "../lib/status";
import {
  INTERACTION_LABEL,
  projectInteraction,
  thisDeviceId,
  type InteractionUiState,
} from "../lib/interactionStatus";
import css from "./ApprovalsPage.module.css";
import type { Interaction, InteractionAnswer } from "../types/interaction";

const KIND_FILTERS = ["all", "approval", "question", "plan-review", "elicitation"] as const;

function preview(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.description;
  if (item.request.kind === "question") return item.carrier === "native-tty"
    ? item.request.fields.map((field) => field.description ?? field.title).join("\n")
    : `问你 ${item.request.fields.length} 题 · AskUserQuestion`;
  if (item.request.kind === "plan-review") return item.request.title;
  return item.request.title;
}

function kindLabel(item: Interaction): string {
  if (item.request.kind === "approval") return item.request.title;
  if (item.request.kind === "question") return item.carrier === "native-tty" ? "终端提问" : "AskUserQuestion";
  if (item.request.kind === "plan-review") return "计划";
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

  const queue = rows.filter((r) => r.uiState === "pending" || r.uiState === "answering" || r.uiState === "paused");
  const departed = rows.filter((r) => r.uiState === "expired" || r.uiState === "superseded");
  const pendingCount = queue.length;

  const respond = (item: Interaction, answer: InteractionAnswer) => {
    void hubStore.respond(item.id, answer);
  };

  const setKind = (id: (typeof KIND_FILTERS)[number]) => {
    const next = new URLSearchParams(params);
    if (id === "all") next.delete("kind");
    else next.set("kind", id);
    setParams(next);
  };

  return (
    <div className={css.page} data-testid="approvals-page">
      <header className={css.top}>
        <h1 className={css.title}>审批中心</h1>
        <div className={css.pending}>
          <span className={css.pendingDot} />
          待处理 {pendingCount}
        </div>
        <div className={css.note}>本设备已处理的会从队列消失</div>
        <div className={css.seg}>
          {KIND_FILTERS.map((id) => (
            <button key={id} type="button" className={`${css.segBtn} ${kind === id ? css.segOn : ""}`} onClick={() => setKind(id)}>
              {id === "all" ? "全部" : id === "approval" ? "审批" : id === "question" ? "提问" : id === "elicitation" ? "表单" : "计划"}
            </button>
          ))}
        </div>
      </header>
      <div className={css.body}>
        <div className={css.filters}>
          {hub.hosts.map((h) => (
            <button
              key={h.id}
              type="button"
              className={`${css.hostBtn} ${hostFilter === h.id ? css.hostOn : ""}`}
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
              type="button"
              className={`${css.hostBtn} ${workspaceFilter === w.id ? css.hostOn : ""}`}
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
        {queue.map(({ item, instance, uiState }) => {
          const focused = focus === item.id;
          const paused = uiState === "paused";
          const answering = uiState === "answering";
          const workspace = hubStore.workspaceOf(instance?.workspaceId ?? "");
          return (
            <article
              key={item.id}
              className={`${css.card} ${uiState === "pending" ? css.cardPending : ""} ${focused ? css.cardFocus : ""}`}
              data-testid="approval-row"
              data-state={uiState}
            >
              {instance ? <StateDot status={projectStatus(instance)} /> : null}
              <div className={css.bodyCol}>
                <div className={css.meta}>
                  <span>{formatClock(item.createdAt)}</span>
                  <span className={css.sep}>·</span>
                  <span className={css.metaHost}>{hubStore.hostName(item.hostId)}</span>
                  <span>
                    / {workspace?.label} / {instance?.kind}
                  </span>
                  <span className={css.sep}>·</span>
                  <span>{INTERACTION_LABEL[uiState as InteractionUiState]}</span>
                </div>
                <div className={css.headline}>
                  <div className={`${css.kind} ${paused ? css.kindMute : ""}`}>{kindLabel(item)}</div>
                  {item.carrier === "native-tty" ? <pre className={css.excerpt}>{preview(item)}</pre>
                    : <p className={`${css.preview} ${paused ? css.previewMute : ""}`}>{preview(item)}</p>}
                </div>
                {item.carrier === "native-tty" ? <p className={css.terminalNote}>来自终端屏幕 · 回答会发送按键</p> : null}
                {item.carrier === "harness-hook" ? <p className={css.terminalNote}>来自工具钩子 · 回答直接决定工具是否执行</p> : null}
                {!item.answerable ? <p className={css.terminalNote}>请打开会话查看完整终端提示</p> : null}
                {paused ? <p className={css.note}>主机离线，交互暂停</p> : null}
                {item.kind === "question" && item.carrier !== "native-tty" && !answering ? (
                  <QuestionForm key={item.id} interaction={item} busy={paused || !item.answerable} onRespond={(answer) => respond(item, answer)} />
                ) : null}
                {item.kind === "elicitation" && !answering ? (
                  <ElicitationCard key={item.id} interaction={item} busy={paused || !item.answerable} onRespond={(answer) => respond(item, answer)} />
                ) : null}
              </div>
              <div className={css.actions}>
                {answering ? (
                  <span className={`${css.btn} ${css.btnDash}`}>
                    <span className={`${css.spin} spin`} />
                    已提交
                  </span>
                ) : null}
                {!answering && item.kind === "approval" && item.request.kind === "approval"
                  ? item.request.options.map((opt) => (
                      <button
                        key={opt.id}
                        type="button"
                        className={`${css.btn} ${opt.effect === "deny" ? "" : css.btnDust} ${paused ? css.btnDash : ""}`}
                        disabled={paused || !item.answerable}
                        onClick={() =>
                          respond(item, {
                            kind: "approval",
                            optionId: opt.id,
                            inputDigest: item.request.kind === "approval" ? item.request.inputDigest : "",
                          })
                        }
                      >
                        {opt.label}
                      </button>
                    ))
                  : null}
                {!answering && item.kind === "question" && item.carrier === "native-tty" ?
                  <PtyQuestionAnswers item={item} disabled={paused || !item.answerable}
                    onAnswer={(answer) => respond(item, answer)} /> : null}
                {!answering && item.kind === "plan-review" && item.request.kind === "plan-review"
                  ? item.request.options.map((opt) => (
                      <button
                        key={opt.id}
                        type="button"
                        className={`${css.btn} ${opt.effect === "deny" ? "" : css.btnDust}`}
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
                      </button>
                    ))
                  : null}
                <Link to={`/s/${item.instanceId}`} className={`${css.btn} ${css.btnMute}`}>
                  打开会话
                </Link>
              </div>
            </article>
          );
        })}
        {departed.length ? (
          <div className={css.departed}>
            <div className={css.departedLabel}>已离队</div>
            {departed.map(({ item, instance, uiState }) => {
              const workspace = hubStore.workspaceOf(instance?.workspaceId ?? "");
              return (
                <article key={item.id} className={css.departedRow} data-testid="approval-row" data-state={uiState}>
                  <div className={css.departedMark}>○</div>
                  <div className={css.departedMeta}>
                    {formatClock(item.createdAt)} · {hubStore.hostName(item.hostId)} / {workspace?.label}
                  </div>
                  <div className={css.departedTitle}>
                    {kindLabel(item)} · {preview(item)}
                  </div>
                  <div className={css.departedState}>{INTERACTION_LABEL[uiState as InteractionUiState]}</div>
                </article>
              );
            })}
          </div>
        ) : null}
        {rows.length === 0 ? <p className={css.empty}>没有待处理交互</p> : null}
      </div>
    </div>
  );
}
