import { useState, type KeyboardEvent } from "react";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import { composing } from "../../lib/viewport";
import css from "../session/session.module.css";

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
  const [answers, setAnswers] = useState<Record<string, { optionIds: string[]; text: string | null }>>({});
  const req = interaction.request;
  if (req.kind !== "question") return null;
  const field = req.fields[index];
  if (!field) return null;
  const current = answers[field.id] ?? { optionIds: [], text: null };
  const disabled = busy || interaction.state !== "pending";

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (composing(event)) return;
    if (event.key === "Enter") event.preventDefault();
  };

  return (
    <section className={css.question} data-testid="question-form">
      <div className={css.approvalHead} style={{ padding: 0, borderBottom: 0 }}>
        <span className={css.dustDot} />
        <span className={css.approvalTitle}>{req.title}</span>
        <span className={css.spacer} />
        <span className={css.approvalHint}>
          {index + 1} / {req.fields.length} · 接管 composer
        </span>
      </div>
      <p className={css.qTitle}>{field.title}</p>
      <div className={css.qOpts}>
        {field.options.map((opt) => {
          const on = current.optionIds.includes(opt.id);
          return (
            <button
              key={opt.id}
              type="button"
              className={`${css.qOpt} ${on ? css.qOptOn : ""}`}
              disabled={disabled}
              onClick={() => {
                const optionIds =
                  field.input === "multi-select"
                    ? current.optionIds.includes(opt.id)
                      ? current.optionIds.filter((id) => id !== opt.id)
                      : current.optionIds.concat(opt.id)
                    : [opt.id];
                setAnswers({ ...answers, [field.id]: { optionIds, text: current.text } });
                if (field.input === "single-select" && index < req.fields.length - 1) setIndex(index + 1);
              }}
            >
              {opt.label}
            </button>
          );
        })}
      </div>
      {field.allowFreeText ? (
        <input
          className={css.input}
          value={current.text ?? ""}
          disabled={disabled}
          placeholder="自定义…（IME 组字不提交）"
          onKeyDown={onKeyDown}
          onChange={(e) => setAnswers({ ...answers, [field.id]: { ...current, text: e.target.value } })}
        />
      ) : null}
      <div className={css.qRow}>
        <button
          type="button"
          className={css.quietBtn}
          disabled={disabled}
          onClick={() => {
            if (index < req.fields.length - 1) setIndex(index + 1);
          }}
        >
          跳过本题
        </button>
        <span className={css.spacer} />
        {index < req.fields.length - 1 ? (
          <button type="button" className={css.allowBtn} onClick={() => setIndex(index + 1)}>
            下一题
          </button>
        ) : (
          <button type="button" className={css.allowBtn} disabled={disabled} onClick={() => onRespond({ kind: "question", answers })}>
            提交
          </button>
        )}
      </div>
      <div className={css.approvalHint}>关闭 = cancel 整批 · 提交一次 InteractionAnswer · 多设备以第一次为准</div>
      <button
        type="button"
        className={css.quietBtn}
        disabled={disabled}
        onClick={() => onRespond({ kind: "question", answers: {} })}
      >
        关闭
      </button>
    </section>
  );
}
