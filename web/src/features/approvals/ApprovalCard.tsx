import { memo, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import type { UiStatus } from "../../types/instance";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import type { InteractionUiState } from "../../lib/interactionStatus";
import { optionAnswerFor } from "./answers";
import {
  QUEUE_STATUS_TEXT,
  carrierLabel,
  deadlineLabel,
  decisionPreview,
  decisionTitle,
  statusDotOf,
} from "./approvalRows";
import { QuestionForm } from "./QuestionForm";
import { ElicitationCard } from "./ElicitationCard";
import { PtyQuestionAnswers } from "../../components/PtyQuestionAnswers";
import css from "./decision.module.css";

export type InboxMode = "desktop" | "compact";

/**
 * The single normalized shape the shared decision card renders. Both row
 * derivations (desktop approvalRows / mobile inboxRows) adapt to it, so the
 * desktop centre and the compact inbox render the SAME card component from
 * the SAME view model (ui-spec §2.5 single shell).
 */
export type DecisionView = {
  key: string;
  /** Deep-equality signature: equal sig skips re-render on a poll re-parse. */
  sig: string;
  item: Interaction;
  uiState: Extract<InteractionUiState, "pending" | "answering" | "paused">;
  focused: boolean;
  timeLabel: string;
  hostLabel: string;
  workspaceLabel: string;
  instanceKind: string;
};

const MAX_FEEDBACK_BYTES = 4 * 1024;

/** Restate an allow-session grant's scope in the harness's own words. */
const GRANT_HINT = "按 harness 建议的范围持续允许";

function hasAllowSession(item: Interaction): boolean {
  if (item.request.kind !== "approval") return false;
  return item.request.options.some((opt) => opt.effect === "allow-session");
}

function StatusLine({
  uiState,
  dot,
}: {
  uiState: DecisionView["uiState"];
  dot: UiStatus;
}) {
  const stateClass =
    uiState === "answering"
      ? css.statusAnswering
      : uiState === "paused"
        ? css.statusPaused
        : css.statusPending;
  return (
    <div className={`${css.status} ${stateClass}`}>
      <StateDot status={dot} title={QUEUE_STATUS_TEXT[uiState]} />
      <p
        className={css.statusText}
        data-testid={uiState === "paused" ? "m-inbox-paused" : undefined}
      >
        {QUEUE_STATUS_TEXT[uiState]}
      </p>
    </div>
  );
}

/**
 * The unified inbox decision card, memoized on `sig`. A 2 s interaction.list
 * poll re-parses unchanged interactions into fresh objects; an equal sig means
 * every render input is unchanged, so the card bails out (c-inboxperf, the
 * scenario-C long-task fix). Local plan-feedback draft survives because the
 * component stays mounted while the sig is equal.
 */
export const DecisionCard = memo(function DecisionCard({
  view,
  mode,
  onRespond,
}: {
  view: DecisionView;
  mode: InboxMode;
  onRespond: (item: Interaction, answer: InteractionAnswer) => void | Promise<void>;
}) {
  const { item, uiState, focused } = view;
  const paused = uiState === "paused";
  const answering = uiState === "answering";
  const compact = mode === "compact";

  const [feedbackText, setFeedbackText] = useState("");
  const [submitError, setSubmitError] = useState<string | null>(null);
  // UTF-8 byte length must match the server's 4 KiB feedback cap (Chinese is
  // 3 bytes/char, so a character count is wrong).
  const feedbackBytes = new TextEncoder().encode(feedbackText).length;
  const feedbackOverLimit = feedbackBytes > MAX_FEEDBACK_BYTES;

  const review =
    item.kind === "plan-review" && item.request.kind === "plan-review" ? item.request : null;

  const submit = async (answer: InteractionAnswer) => {
    setSubmitError(null);
    try {
      await onRespond(item, answer);
    } catch (error) {
      // The store clears the local answering flag on a rejected POST, so the
      // card is answerable again; surface the error and keep the draft.
      setSubmitError(error instanceof Error ? error.message : String(error));
    }
  };

  const blocked = paused || !item.answerable;
  const previewText = decisionPreview(item);
  // The verbatim <pre> carries approvals and native-tty questions. Hook
  // questions render the tabbed form and plan reviews / elicitations their
  // own body, so the title is not duplicated in a preview block.
  const showPreview =
    item.kind === "approval" || (item.kind === "question" && item.carrier === "native-tty");
  const showGrantHint = hasAllowSession(item);
  const location = view.workspaceLabel || view.hostLabel || "未知";

  // Compact questions leave for the session (too long to finish in a list
  // row); that path renders a single 去回答 link and nothing else.
  const compactQuestion = compact && item.kind === "question";

  const renderOptions = () => {
    if (item.request.kind === "approval") {
      return item.request.options.map((opt) => (
        <button
          key={opt.id}
          type="button"
          className={opt.effect === "deny" ? css.actionBtn : css.actionPrimary}
          disabled={blocked}
          onClick={() => {
            const answer = optionAnswerFor(item, opt.id);
            if (answer) void submit(answer);
          }}
        >
          {opt.label}
        </button>
      ));
    }
    if (review) {
      return review.options.map((opt) => {
        const isDeny = opt.effect === "deny" || opt.id === "deny";
        return (
          <button
            key={opt.id}
            type="button"
            className={isDeny ? css.actionBtn : css.actionPrimary}
            disabled={blocked || (isDeny && feedbackOverLimit)}
            onClick={() =>
              void submit({
                kind: "plan-review",
                optionId: opt.id,
                planRevision: review.planRevision,
                planDigest: review.planDigest,
                // Null only when the field is literally empty; a supplied
                // whitespace note is forwarded verbatim.
                feedback: isDeny && feedbackText.length > 0 ? feedbackText : null,
              })
            }
          >
            {opt.label}
          </button>
        );
      });
    }
    return null;
  };

  return (
    <article
      className={`${compact ? css.cardCompact : css.card} ${focused ? css.cardFocus : ""}`}
      data-testid="approval-row"
      data-interaction-id={item.id}
      data-kind={item.kind}
      data-state={uiState}
      data-focus={focused ? "true" : "false"}
    >
      <div className={css.context}>
        <span className={css.contextTime}>{view.timeLabel}</span>
        <span className={css.contextSep} aria-hidden>
          ·
        </span>
        <span className={css.contextHost}>{view.hostLabel}</span>
        <span>
          / {view.workspaceLabel || "—"} / {view.instanceKind}
        </span>
      </div>

      <StatusLine uiState={uiState} dot={statusDotOf(uiState)} />

      <h3 className={css.title}>{decisionTitle(item)}</h3>

      {showPreview ? <pre className={css.preview}>{previewText}</pre> : null}

      <dl className={css.facts}>
        <dt className={css.factTerm}>来源</dt>
        <dd className={css.factData}>{carrierLabel(item.carrier)}</dd>
        <dt className={css.factTerm}>截止</dt>
        <dd className={css.factData} data-testid="approval-deadline">
          {deadlineLabel(item)}
        </dd>
        <dt className={css.factTerm}>位置</dt>
        <dd className={css.factData}>{location}</dd>
      </dl>

      {item.carrier === "native-tty" ? (
        <p className={css.note}>来自终端屏幕 · 回答会发送按键</p>
      ) : null}
      {item.carrier === "harness-hook" ? (
        <p className={css.note}>来自工具钩子 · 回答直接决定工具是否执行</p>
      ) : null}
      {!item.answerable ? <p className={css.note}>请打开会话查看完整终端提示</p> : null}

      {/* Question / elicitation body. Hook questions are inline on desktop
          and a 去回答 link on compact; native-tty questions use quick keys. */}
      {!answering && !compactQuestion && item.kind === "question" ? (
        item.carrier === "native-tty" ? (
          <PtyQuestionAnswers
            item={item}
            disabled={blocked}
            onAnswer={(answer) => void submit(answer)}
          />
        ) : (
          <QuestionForm
            embedded
            interaction={item}
            busy={blocked}
            onRespond={(answer) => void submit(answer)}
          />
        )
      ) : null}
      {!answering && !compactQuestion && item.kind === "elicitation" ? (
        <ElicitationCard
          embedded
          interaction={item}
          busy={blocked}
          onRespond={(answer) => void submit(answer)}
        />
      ) : null}

      {review ? (
        typeof review.plan === "string" && review.plan.length > 0 ? (
          <details className={css.planDetails}>
            <summary className={css.planSummary}>查看计划（{review.plan.length} 字）</summary>
            <pre className={css.planBody}>{review.plan}</pre>
          </details>
        ) : (
          <p className={css.note}>
            计划正文未随请求内联提供，请
            <Link to={`/s/${item.instanceId}`} className={css.planSessionLink}>
              打开会话查看
            </Link>
            完整计划后再决定
          </p>
        )
      ) : null}

      {submitError ? (
        <p className={css.planError} role="alert">
          {submitError}
        </p>
      ) : null}

      {!compactQuestion && !answering && review && review.allowFeedback ? (
        <textarea
          className={`${css.planFeedback} ${feedbackOverLimit ? css.planFeedbackInvalid : ""}`}
          aria-label="计划审批拒绝反馈（可选）"
          aria-invalid={feedbackOverLimit}
          rows={2}
          placeholder="拒绝时回给子代理的反馈（可选）"
          value={feedbackText}
          disabled={blocked}
          onChange={(event) => {
            setFeedbackText(event.target.value);
            setSubmitError(null);
          }}
        />
      ) : null}
      {!compactQuestion && !answering && review && review.allowFeedback && feedbackOverLimit ? (
        <p className={css.planError} role="alert">
          反馈超过 {MAX_FEEDBACK_BYTES} 字节（当前 {feedbackBytes}），请缩短后再拒绝
        </p>
      ) : null}

      <div className={`${css.actions} ${compact ? css.actionsCompact : ""}`}>
        {answering ? (
          <button
            type="button"
            className={css.submitting}
            data-testid="approval-submitting"
            disabled
          >
            <span className={css.spin} aria-hidden />
            {QUEUE_STATUS_TEXT.answering}
          </button>
        ) : compactQuestion ? (
          <Link
            to={`/s/${item.instanceId}`}
            className={css.actionPrimary}
            data-testid="m-inbox-answer"
          >
            去回答
          </Link>
        ) : (
          renderOptions()
        )}
        {!answering && !compactQuestion && showGrantHint ? (
          <span className={css.grantHint}>{GRANT_HINT}</span>
        ) : null}
        {!compactQuestion ? (
          <Link to={`/s/${item.instanceId}`} className={css.openLink}>
            打开会话
          </Link>
        ) : null}
      </div>
    </article>
  );
}, areEqual);

function areEqual(
  prev: { view: DecisionView; mode: InboxMode; onRespond: (i: Interaction, a: InteractionAnswer) => void | Promise<void> },
  next: typeof prev,
): boolean {
  // The adapters allocate a fresh view object every poll; compare its sig
  // (which covers every rendered field) instead of object identity.
  return prev.view.sig === next.view.sig && prev.mode === next.mode && prev.onRespond === next.onRespond;
}

/**
 * Standalone approval card pinned above the session composer (SessionPage).
 * Approval-only; renders the verbatim tool input and the harness's own option
 * labels, never rewritten. Visuals share the decision shell.
 */
export function ApprovalCard({
  interaction,
  busy,
  onRespond,
}: {
  interaction: Interaction;
  busy?: boolean;
  onRespond: (answer: InteractionAnswer) => void;
}) {
  const req = interaction.request;
  if (req.kind !== "approval") return null;
  const disabled = busy || !interaction.answerable || interaction.state !== "pending";
  return (
    <section className={css.approval} data-testid="approval-card">
      <div className={css.approvalHead}>
        <span className={css.dustDot} />
        <span className={css.approvalTitle}>审批 · {req.title}</span>
        <span className={css.approvalHint}>{interaction.id.slice(0, 12)}</span>
        <span className={css.spacer} />
        <span className={css.approvalHint}>多台设备同时点，只记第一次</span>
      </div>
      <div className={css.approvalBody}>
        <p className={css.preview}>{req.description}</p>
        {req.options.map((opt) => {
          const kind = opt.effect === "deny" ? css.denyBtn : css.allowBtn;
          const answer = optionAnswerFor(interaction, opt.id);
          return (
            <button
              key={opt.id}
              type="button"
              className={kind}
              disabled={disabled || !answer}
              onClick={() => answer && onRespond(answer)}
            >
              {opt.label}
            </button>
          );
        })}
        {hasAllowSession(interaction) ? <p className={css.grantHint}>{GRANT_HINT}</p> : null}
      </div>
    </section>
  );
}
