import { memo, useState } from "react";
import { Link } from "react-router-dom";
import { RootTurnTag } from "../../components/RootTurnTag";
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
import { InteractionDeadline } from "./InteractionDeadline";
import { PtyQuestionAnswers } from "../../components/PtyQuestionAnswers";
import {
  HOME_RECEIPT_TEXT,
  HOME_STALE_TEXT,
  chipAnswer,
  homeApprovalOptions,
  homeCardKind,
  inboxHref,
  lastLines,
  type HomeCardKind,
} from "../home/homeQueue";
import css from "./decision.module.css";
import homeCss from "../home/homeQueue.module.css";

/**
 * `home` is the ④ 需要你 card (home.md §4.2): it grants less than the inbox
 * (§4.3) — see {@link HomeDecision}.
 */
export type InboxMode = "desktop" | "compact" | "home";

/** What only a `mode="home"` card needs from its queue (home.md §4.4, §8). */
export type HomeCardState = {
  placement: "phone" | "desktop";
  /** §4.4 receipt: 「已提交」 after a 200, 「已在其他设备处理」 after 409/410. */
  receipt: "submitted" | "elsewhere" | null;
  /** §8 stale: buttons grey out with 「数据可能过期」. */
  stale: boolean;
  /** Painted during a §3.3 hold but gone from the live queue: nothing fires. */
  inert?: boolean;
  /** The tty card's last screen lines (§4.2 终端提示); empty = request text. */
  screenLines?: readonly string[];
};

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
  home,
}: {
  view: DecisionView;
  mode: InboxMode;
  onRespond: (item: Interaction, answer: InteractionAnswer) => void | Promise<void>;
  /** Required by `mode="home"`; ignored by the inbox modes. */
  home?: HomeCardState;
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

  if (mode === "home") {
    return (
      <HomeDecision
        view={view}
        home={home ?? { placement: "phone", receipt: null, stale: false }}
        submitError={submitError}
        submit={submit}
      />
    );
  }

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
        <RootTurnTag rootTurn={item.rootTurn} surface="approvals" />
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
            <summary className={css.planSummary}>
              {/* c-inboxfu: the summary is display:flex, which drops the
                  native disclosure widget in some engines; paint the marker
                  ourselves (same ▸/▾ convention as runDetails) so the
                  affordance is visible in WebKit and Chromium. */}
              <span className={css.planChev} aria-hidden="true" />
              查看计划（{review.plan.length} 字）
            </summary>
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

      {/* c-question-alert: only the compact QUESTION row needs its own
          deadline node — it deep-links with 去回答 and never mounts the form.
          A compact elicitation row renders <ElicitationCard> above (it is
          not a compactQuestion), which already carries InteractionDeadline;
          r2 rendered it twice for that kind. */}
      {compactQuestion ? (
        <InteractionDeadline interaction={item} testid="m-inbox-deadline" />
      ) : null}

      {review ? <InteractionDeadline interaction={item} /> : null}

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
            // ui-spec §2.5 names the compact state m-inbox-submitting; the
            // desktop centre uses the approval- prefixed id. Both are the same
            // disabled, single control — a second answer cannot be POSTed.
            data-testid={compact ? "m-inbox-submitting" : "approval-submitting"}
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
  prev: {
    view: DecisionView;
    mode: InboxMode;
    onRespond: (i: Interaction, a: InteractionAnswer) => void | Promise<void>;
    home?: HomeCardState;
  },
  next: typeof prev,
): boolean {
  // The adapters allocate a fresh view object every poll; compare its sig
  // (which covers every rendered field) instead of object identity.
  return (
    prev.view.sig === next.view.sig
    && prev.mode === next.mode
    && prev.onRespond === next.onRespond
    && sameHome(prev.home, next.home)
  );
}

function sameHome(a: HomeCardState | undefined, b: HomeCardState | undefined): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.placement === b.placement
    && a.receipt === b.receipt
    && a.stale === b.stale
    && (a.inert ?? false) === (b.inert ?? false)
    && (a.screenLines ?? []).join("\n") === (b.screenLines ?? []).join("\n")
  );
}

/** The kind label on a home card's first line (home.md §2.1 wireframe). */
function homeLabel(item: Interaction, kind: HomeCardKind): string {
  if (kind === "approval") {
    return item.request.kind === "approval" ? `需要你授权 · ${item.request.title}` : "需要你授权";
  }
  if (kind === "plan") return "计划待审";
  if (kind === "tty") return "终端提示";
  if (kind === "sensitive") return item.kind === "elicitation" ? "需要你填写" : "需要你回答 · 敏感";
  return "需要你回答";
}

/** The card's body text, verbatim (ui-spec §2.5: never rewritten). */
function homeBody(item: Interaction, kind: HomeCardKind, screenLines: readonly string[]): string {
  if (kind === "tty") {
    const screen = lastLines(screenLines);
    if (screen.length) return screen.join("\n");
    return lastLines(decisionPreview(item).split("\n")).join("\n");
  }
  if (item.request.kind === "question") {
    const fields = item.request.fields;
    if (fields.length === 1) return fields[0].title || fields[0].description || item.request.title;
    return `问你 ${fields.length} 题 · ${item.request.title}`;
  }
  if (item.request.kind === "plan-review") {
    const plan = typeof item.request.plan === "string" ? lastLinesHead(item.request.plan, 2) : "";
    return plan ? `${item.request.title}\n${plan}` : item.request.title;
  }
  return decisionPreview(item);
}

/** The first `count` non-blank lines of a text. */
function lastLinesHead(text: string, count: number): string {
  return text
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .slice(0, count)
    .join("\n");
}

/**
 * The ④ 需要你 card (home.md §4.2–4.4, `mode="home"`). Smaller surfaces grant
 * less (§4.3): an approval offers only its `deny` and `allow-once` options
 * (allow-session is never rendered; with either missing the card is a
 * 去收件箱 link), a simple question gets chips plus 「其他…」, every other
 * question 去回答, a plan only 审阅计划 — there is no approve control on the
 * home at all — a terminal prompt its last 3 lines plus 去终端, and a
 * sensitive field or an elicitation only 去会话. No batch control exists.
 *
 * The answer flow is not optimistic (§4.4): buttons stay disabled while the
 * POST is in flight; the queue owns the receipt (「已提交」 / 「已在其他设备处理」),
 * and any other error is shown under the buttons, which re-enable.
 */
function HomeDecision({
  view,
  home,
  submitError,
  submit,
}: {
  view: DecisionView;
  home: HomeCardState;
  submitError: string | null;
  submit: (answer: InteractionAnswer) => Promise<void>;
}) {
  const { item, uiState, focused } = view;
  const kind = homeCardKind(item);
  const answering = uiState === "answering";
  const paused = uiState === "paused";
  const inert = home.inert === true;
  // §8: a stale queue greys every answer control; answering, paused and the
  // receipt keep them off too. Links stay live — they only navigate.
  const disabled = answering || paused || home.stale || inert || home.receipt !== null || !item.answerable;
  const where = [view.hostLabel, view.workspaceLabel].filter(Boolean).join(" › ");
  const session = `/s/${item.instanceId}`;
  const linkProps = inert
    ? { "aria-disabled": true, tabIndex: -1, onClick: (event: { preventDefault: () => void }) => event.preventDefault() }
    : {};

  const actions = (() => {
    if (kind === "approval") {
      const options = homeApprovalOptions(item);
      if (!options) {
        return (
          <Link to={inboxHref(home.placement, item.id)} className={css.actionBtn} data-testid="home-card-inbox" {...linkProps}>
            去收件箱
          </Link>
        );
      }
      return [options.deny, options.allowOnce].map((opt) => (
        <button
          key={opt.id}
          type="button"
          className={opt.effect === "deny" ? css.actionBtn : css.actionPrimary}
          data-testid="home-card-option"
          data-effect={opt.effect}
          disabled={disabled}
          onClick={() => {
            const answer = optionAnswerFor(item, opt.id);
            if (answer) void submit(answer);
          }}
        >
          {opt.label}
        </button>
      ));
    }
    if (kind === "simple-question" && item.request.kind === "question") {
      return [
        ...item.request.fields[0].options.map((opt) => (
          <button
            key={opt.id}
            type="button"
            className={homeCss.chip}
            data-testid="home-card-chip"
            // Single-line control (visual-system §8.1): a clipped label keeps
            // its full text in the tooltip.
            title={opt.label}
            disabled={disabled}
            onClick={() => {
              const answer = chipAnswer(item, opt.id);
              if (answer) void submit(answer);
            }}
          >
            {opt.label}
          </button>
        )),
        <Link key="other" to={session} className={homeCss.chipLink} data-testid="home-card-other" {...linkProps}>
          其他…
        </Link>,
      ];
    }
    const link =
      kind === "plan"
        ? { to: session, text: "审阅计划", testid: "home-card-review" }
        : kind === "tty"
          ? { to: `/s/${item.instanceId}/tty`, text: "去终端", testid: "home-card-tty" }
          : kind === "sensitive"
            ? { to: session, text: "去会话", testid: "home-card-session" }
            : { to: session, text: "去回答", testid: "home-card-answer" };
    return (
      <Link to={link.to} className={css.actionPrimary} data-testid={link.testid} {...linkProps}>
        {link.text}
      </Link>
    );
  })();

  return (
    <article
      className={`${homeCss.card} ${focused ? homeCss.cardFocus : ""}`}
      data-testid="home-queue-card"
      data-card-type="interaction"
      data-card-id={item.id}
      data-interaction-id={item.id}
      data-kind={item.kind}
      data-home-kind={kind}
      data-state={uiState}
      data-focus={focused ? "true" : "false"}
      data-receipt={home.receipt ?? undefined}
      data-stale={inert ? "1" : undefined}
    >
      <header className={homeCss.head}>
        <span className={homeCss.label} title={homeLabel(item, kind)}>
          {homeLabel(item, kind)}
        </span>
        {where ? (
          <span className={homeCss.where} title={where}>
            {where}
          </span>
        ) : null}
      </header>
      <p className={kind === "tty" || kind === "approval" ? homeCss.bodyMono : homeCss.body} data-testid="home-card-body">
        {homeBody(item, kind, home.screenLines ?? [])}
      </p>
      {home.receipt ? (
        <p className={homeCss.receipt} role="status" data-testid="home-card-receipt" data-receipt={home.receipt}>
          {HOME_RECEIPT_TEXT[home.receipt]}
        </p>
      ) : (
        <div className={homeCss.actions}>
          {actions}
          {answering ? (
            <span className={homeCss.hint} data-testid="home-card-submitting">
              {QUEUE_STATUS_TEXT.answering}
            </span>
          ) : paused ? (
            <span className={homeCss.hint}>{QUEUE_STATUS_TEXT.paused}</span>
          ) : home.stale ? (
            <span className={homeCss.hint} data-testid="home-card-stale">
              {HOME_STALE_TEXT}
            </span>
          ) : null}
        </div>
      )}
      {submitError && !home.receipt ? (
        <p className={css.planError} role="alert" data-testid="home-card-error">
          {submitError}
        </p>
      ) : null}
    </article>
  );
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
