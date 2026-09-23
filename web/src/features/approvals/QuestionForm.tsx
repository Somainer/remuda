import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { Interaction, InteractionAnswer, QuestionField } from "../../types/interaction";
import { composing } from "../../lib/viewport";
import css from "./decision.module.css";

type FieldAnswer = { optionIds: string[]; text: string | null };

/** Tab title: the question header, falling back to a numbered label. */
function tabTitle(field: QuestionField, index: number): string {
  return field.description?.trim() || `问题 ${index + 1}`;
}

function isAnswered(value: FieldAnswer | undefined): boolean {
  if (!value) return false;
  return value.optionIds.length > 0 || (value.text?.trim().length ?? 0) > 0;
}

/**
 * AskUserQuestion card, mirroring the harness TUI: one tab per question
 * (header as title), radio / checkbox options with descriptions, a
 * "Type something" free-text row per question, and a single Submit that
 * answers the whole batch. Raw JSON is available only behind 「原始」.
 */
export function QuestionForm({
  interaction,
  busy,
  onRespond,
}: {
  interaction: Interaction;
  busy?: boolean;
  onRespond: (answer: InteractionAnswer) => void;
}) {
  const [index, setIndex] = useState(0);
  const [answers, setAnswers] = useState<Record<string, FieldAnswer>>({});
  const [showRaw, setShowRaw] = useState(false);
  const rootRef = useRef<HTMLElement>(null);
  const req = interaction.request;
  // Focus the card on mount so its digit/Enter shortcuts work without a click,
  // like the TUI form that owns input while it is open. Sync-only, no state.
  useEffect(() => {
    rootRef.current?.focus();
  }, []);
  if (req.kind !== "question") return null;
  const fields = req.fields;
  const field = fields[index];
  if (!field) return null;
  const current: FieldAnswer = answers[field.id] ?? { optionIds: [], text: null };
  const disabled = busy || interaction.state !== "pending";
  const complete = fields.every((f) => isAnswered(answers[f.id]));

  const patch = (next: FieldAnswer) => setAnswers((prev) => ({ ...prev, [field.id]: next }));

  const choose = (optionId: string) => {
    if (disabled) return;
    if (field.input === "multi-select") {
      const on = current.optionIds.includes(optionId);
      patch({
        optionIds: on
          ? current.optionIds.filter((id) => id !== optionId)
          : current.optionIds.concat(optionId),
        // An option choice replaces any free text, as in the TUI.
        text: null,
      });
    } else {
      patch({ optionIds: [optionId], text: null });
    }
  };

  const typeFree = (text: string) => patch({ optionIds: [], text: text.length ? text : null });

  const submit = () => {
    if (disabled || !complete) return;
    onRespond({ kind: "question", answers });
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (composing(event)) return;
    const target = event.target as HTMLElement;
    const typing = target.tagName === "INPUT" || target.tagName === "TEXTAREA";
    // Digits select while not typing free text.
    if (!typing && /^[1-9]$/.test(event.key)) {
      const option = field.options[Number(event.key) - 1];
      if (option) {
        event.preventDefault();
        choose(option.id);
        // Single-select advances to the next question, like the TUI.
        if (field.input !== "multi-select" && index < fields.length - 1) setIndex(index + 1);
      }
      return;
    }
    if (event.key === "Enter" && !typing) {
      event.preventDefault();
      submit();
    }
  };

  return (
    <section
      ref={rootRef}
      tabIndex={-1}
      className={css.question}
      data-testid="question-form"
      onKeyDown={onKeyDown}
    >
      <div className={css.approvalHead} style={{ padding: 0, borderBottom: 0 }}>
        <span className={css.dustDot} />
        <span className={css.approvalTitle}>{req.title}</span>
        <span className={css.spacer} />
        <span className={css.approvalHint}>
          {fields.length} 题 · Enter 提交 · 数字键选择
        </span>
      </div>

      {fields.length > 1 ? (
        <div className={css.qTabs} role="tablist" aria-label="问题">
          {fields.map((f, i) => (
            <button
              key={f.id}
              type="button"
              role="tab"
              aria-selected={i === index}
              className={`${css.qTab} ${i === index ? css.qTabOn : ""}`}
              disabled={disabled}
              data-testid={`question-tab-${f.id}`}
              onClick={() => setIndex(i)}
            >
              <span
                className={isAnswered(answers[f.id]) ? css.qTabDotOn : css.qTabDot}
                aria-hidden
              />
              {tabTitle(f, i)}
            </button>
          ))}
        </div>
      ) : null}

      <div className={css.qPanel} role="group" aria-label={tabTitle(field, index)}>
        <p className={css.qTitle}>{field.title}</p>
        <div className={css.qOptList} role={field.input === "multi-select" ? "group" : "radiogroup"}>
          {field.options.map((opt, i) => {
            const on = current.optionIds.includes(opt.id);
            return (
              <button
                key={opt.id}
                type="button"
                role={field.input === "multi-select" ? "checkbox" : "radio"}
                aria-checked={on}
                className={`${css.qOptRow} ${on ? css.qOptOn : ""}`}
                disabled={disabled}
                data-testid={`question-option-${field.id}-${opt.id}`}
                onClick={() => choose(opt.id)}
              >
                <span aria-hidden className={field.input === "multi-select" ? (on ? css.qCheckOn : css.qCheck) : (on ? css.qRadioOn : css.qRadio)} />
                <span className={css.qOptText}>
                  <span className={css.qOptLabel}>
                    <span className={css.qOptNum}>{i + 1}</span>
                    {opt.label}
                  </span>
                  {opt.description ? <span className={css.qOptDesc}>{opt.description}</span> : null}
                </span>
              </button>
            );
          })}
        </div>
        {field.allowFreeText ? (
          <label className={css.qFree}>
            <span
              aria-hidden
              className={current.text ? css.qCheckOn : css.qCheck}
            />
            <span className={css.qFreeTag}>其他</span>
            <input
              className={css.input}
              value={current.text ?? ""}
              disabled={disabled}
              placeholder="Type something…"
              data-testid={`question-free-${field.id}`}
              onKeyDown={(event) => {
                if (composing(event)) return;
                if (event.key === "Enter") {
                  event.preventDefault();
                  if (index < fields.length - 1) setIndex(index + 1);
                  else submit();
                }
              }}
              onChange={(event) => typeFree(event.target.value)}
            />
          </label>
        ) : null}
      </div>

      <div className={css.qRow}>
        <button
          type="button"
          className={css.quietBtn}
          disabled={disabled}
          data-testid="question-deny"
          onClick={() => onRespond({ kind: "question", answers: {} })}
        >
          拒绝
        </button>
        <span className={css.spacer} />
        <details
          className={css.qRaw}
          open={showRaw}
          onToggle={(event) => setShowRaw((event.target as HTMLDetailsElement).open)}
        >
          <summary data-testid="question-raw-toggle">原始</summary>
          {showRaw ? (
            <pre className={css.qRawPre} data-testid="question-raw">
              {JSON.stringify(req, null, 2)}
            </pre>
          ) : null}
        </details>
        <span className={css.spacer} />
        {index > 0 ? (
          <button type="button" className={css.quietBtn} disabled={disabled} onClick={() => setIndex(index - 1)}>
            上一题
          </button>
        ) : null}
        {index < fields.length - 1 ? (
          <button
            type="button"
            className={css.quietBtn}
            disabled={disabled || !isAnswered(current)}
            onClick={() => setIndex(index + 1)}
          >
            下一题
          </button>
        ) : null}
        <button
          type="button"
          className={css.allowBtn}
          disabled={disabled || !complete}
          data-testid="question-submit"
          onClick={submit}
        >
          提交
        </button>
      </div>
      <div className={css.approvalHint}>提交一次 InteractionAnswer · 多设备以第一次为准</div>
    </section>
  );
}
