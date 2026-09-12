import { useState, type KeyboardEvent } from "react";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import { Button } from "../../components/Button";
import { composing } from "../../lib/viewport";
import ui from "../../styles/ui.module.css";

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
    <section className={ui.approval} data-testid="question-form">
      <div className={ui.cardHead}>
        <strong>{req.title}</strong>
        <span>
          {index + 1}/{req.fields.length}
        </span>
      </div>
      <p>{field.title}</p>
      {field.options.map((opt) => (
        <label key={opt.id} className={ui.row} style={{ marginBottom: 6 }}>
          <input
            type={field.input === "multi-select" ? "checkbox" : "radio"}
            name={field.id}
            checked={current.optionIds.includes(opt.id)}
            disabled={disabled}
            onChange={() => {
              const optionIds = field.input === "multi-select"
                ? current.optionIds.includes(opt.id)
                  ? current.optionIds.filter((id) => id !== opt.id)
                  : current.optionIds.concat(opt.id)
                : [opt.id];
              setAnswers({ ...answers, [field.id]: { optionIds, text: current.text } });
              if (field.input === "single-select" && index < req.fields.length - 1) setIndex(index + 1);
            }}
          />
          {opt.label}
        </label>
      ))}
      {field.allowFreeText ? (
        <input
          className={ui.input}
          value={current.text ?? ""}
          disabled={disabled}
          onKeyDown={onKeyDown}
          onChange={(e) => setAnswers({ ...answers, [field.id]: { ...current, text: e.target.value } })}
        />
      ) : null}
      <div className={ui.row} style={{ marginTop: 8 }}>
        <Button
          disabled={disabled || index === 0}
          onClick={() => setIndex(Math.max(0, index - 1))}
        >
          上一题
        </Button>
        <Button
          disabled={disabled}
          onClick={() => {
            if (index < req.fields.length - 1) setIndex(index + 1);
          }}
        >
          Skip
        </Button>
        {index < req.fields.length - 1 ? (
          <Button onClick={() => setIndex(index + 1)}>下一题</Button>
        ) : (
          <Button
            variant="primary"
            disabled={disabled}
            onClick={() => onRespond({ kind: "question", answers })}
          >
            提交
          </Button>
        )}
        <Button
          variant="danger"
          disabled={disabled}
          onClick={() => onRespond({ kind: "question", answers: {} })}
        >
          关闭
        </Button>
      </div>
    </section>
  );
}
