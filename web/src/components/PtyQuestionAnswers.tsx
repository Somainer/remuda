import { useState } from "react";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import css from "../features/approvals/decision.module.css";

/**
 * One-field native-tty question quick answers (ui-spec §2.5): option keys are
 * sent as keystrokes, plus an optional free-text reply. Rendered inside the
 * unified decision card. The shared .btn control gives a 32px target on fine
 * pointers and 44px on coarse ones (--control-h).
 */
export function PtyQuestionAnswers({ item, disabled, onAnswer }: {
  item: Interaction;
  disabled: boolean;
  onAnswer: (answer: InteractionAnswer) => void;
}) {
  const [text, setText] = useState("");
  if (item.request.kind !== "question" || item.request.fields.length !== 1) return null;
  const field = item.request.fields[0];
  const reply = (optionIds: string[], value: string | null) => onAnswer({
    kind: "question", answers: { [field.id]: { optionIds, text: value } },
  });
  return <div className={css.pty}>
    {field.options.map((option) => <button key={option.id} type="button" className={css.actionBtn} disabled={disabled}
      onClick={() => reply([option.id], null)}>{option.label}</button>)}
    {field.allowFreeText ? <form onSubmit={(event) => { event.preventDefault(); if (text && !disabled) reply([], text); }}>
      <input className={css.ptyInput} aria-label="终端回答" value={text} maxLength={1024} disabled={disabled}
        onChange={(event) => setText(event.target.value)} />
      <button type="submit" className={css.actionPrimary} disabled={disabled || !text}>发送回答</button>
    </form> : null}
  </div>;
}
