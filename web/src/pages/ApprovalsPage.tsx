import { memo, useCallback, useMemo } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { StateDot } from "../components/StateDot";
import { PtyQuestionAnswers } from "../components/PtyQuestionAnswers";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { ElicitationCard } from "../features/approvals/ElicitationCard";
import { deriveApprovalRows, type ApprovalRow } from "../features/approvals/approvalRows";
import { useIncrementalLimit } from "../lib/useIncrementalLimit";
import { formatClock } from "../lib/format";
import { hubStore, useHub } from "../lib/store";
import { projectStatus } from "../lib/status";
import { INTERACTION_LABEL, thisDeviceId } from "../lib/interactionStatus";
import css from "./ApprovalsPage.module.css";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import { profileRegion } from "../lib/profileFlags";

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

type RespondFn = (item: Interaction, answer: InteractionAnswer) => void;

/**
 * Memoized on row.sig (see approvalRows.ts): a 2 s interaction.list poll
 * re-parses unchanged interactions into fresh objects; the sig proves the
 * card's render inputs are equal, so the card does not re-render. That
 * full-list re-commit was the ~2.7 s scenario-C long task (inbox-perf-1).
 */
const QueueCard = memo(
  function QueueCard({ row, onRespond }: { row: ApprovalRow; onRespond: RespondFn }) {
    const { item, instance, uiState, focused } = row;
    const paused = uiState === "paused";
    const answering = uiState === "answering";
    const workspace = hubStore.workspaceOf(instance?.workspaceId ?? "");
    return (
      <article

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
            <span>{INTERACTION_LABEL[uiState]}</span>
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
            <QuestionForm key={item.id} interaction={item} busy={paused || !item.answerable} onRespond={(answer) => onRespond(item, answer)} />
          ) : null}
          {item.kind === "elicitation" && !answering ? (
            <ElicitationCard key={item.id} interaction={item} busy={paused || !item.answerable} onRespond={(answer) => onRespond(item, answer)} />
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
                    onRespond(item, {
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
              onAnswer={(answer) => onRespond(item, answer)} /> : null}
          {!answering && item.kind === "plan-review" && item.request.kind === "plan-review"
            ? item.request.options.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.btn} ${opt.effect === "deny" ? "" : css.btnDust}`}
                  disabled={paused || answering}
                  onClick={() =>
                    onRespond(item, {
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
  },
  (prev, next) => prev.row.sig === next.row.sig && prev.onRespond === next.onRespond,
);

const DepartedRow = memo(
  function DepartedRow({ row }: { row: ApprovalRow }) {
    const { item, instance, uiState } = row;
    const workspace = hubStore.workspaceOf(instance?.workspaceId ?? "");
    return (
      <article className={css.departedRow} data-testid="approval-row" data-state={uiState}>
        <div className={css.departedMark}>○</div>
        <div className={css.departedMeta}>
          {formatClock(item.createdAt)} · {hubStore.hostName(item.hostId)} / {workspace?.label}
        </div>
        <div className={css.departedTitle}>
          {kindLabel(item)} · {preview(item)}
        </div>
        <div className={css.departedState}>{INTERACTION_LABEL[uiState]}</div>
      </article>
    );
  },
  (prev, next) => prev.row.sig === next.row.sig,
);

export function ApprovalsPage() {
  const hub = useHub();
  const [params, setParams] = useSearchParams();
  const focus = params.get("focus");
  const kind = (params.get("kind") as (typeof KIND_FILTERS)[number] | null) ?? "all";
  const hostFilter = params.get("host") ?? "";
  const workspaceFilter = params.get("workspace") ?? "";
  const deviceId = useMemo(() => thisDeviceId(), []);
  // Explicit lookup so the derive memo depends on a stable map rather than on
  // the whole hub (and the dependency is real to the linter: cards read the
  // label through it, so a workspace-catalog refresh must re-derive).
  const workspaceById = useMemo(
    () => new Map(hub.workspaces.map((workspace) => [workspace.id, workspace])),
    [hub.workspaces],
  );

  const rows = useMemo(
    () =>
      profileRegion("approvals.deriveRows", () =>
        deriveApprovalRows(
          {
            interactions: hub.interactions,
            instances: hub.instances,
            hosts: hub.hosts,
            answering: hub.answering,
            deviceId,
            workspaceLabel: (id) => workspaceById.get(id)?.label ?? "",
          },
          { kind, hostId: hostFilter, workspaceId: workspaceFilter, focus },
        ),
      ),
    // Concrete slices, never the whole hub object: an unrelated emission (e.g.
    // a hosts-only poll) must not re-derive when these references are intact.
    [
      hub.interactions,
      hub.instances,
      hub.hosts,
      hub.answering,
      workspaceById,
      deviceId,
      kind,
      hostFilter,
      workspaceFilter,
      focus,
    ],
  );

  const { queue, departed } = rows;
  const pendingCount = queue.length;

  // Progressive mount: committing 100 cards in one task was the scenario-C
  // long task; reveal another slice per animation frame. All rows still mount
  // (counts/deep links/tests see the full list) just never in one task.
  // resetKey is the filter identity: answering a card (total shrinks) keeps
  // revealed rows mounted, but switching filters shows a different set and so
  // restarts slicing.
  const filterKey = `${kind}\u0000${hostFilter}\u0000${workspaceFilter}`;
  const queueLimit = useIncrementalLimit(queue.length, { resetKey: filterKey });
  const departedLimit = useIncrementalLimit(departed.length, { resetKey: filterKey });

  const respond = useCallback<RespondFn>((item, answer) => {
    void hubStore.respond(item.id, answer);
  }, []);

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
        {queue.slice(0, queueLimit).map((row) => (
          <QueueCard key={row.item.id} row={row} onRespond={respond} />
        ))}
        {departed.length ? (
          <div className={css.departed}>
            <div className={css.departedLabel}>已离队</div>
            {departed.slice(0, departedLimit).map((row) => (
              <DepartedRow key={row.item.id} row={row} />
            ))}
          </div>
        ) : null}
        {queue.length + departed.length === 0 ? <p className={css.empty}>没有待处理交互</p> : null}
      </div>
    </div>
  );
}
