import { useState } from "react";
import type { Interaction, InteractionAnswer } from "../types/interaction";

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
  return <div>
    {field.options.map((option) => <button key={option.id} type="button" disabled={disabled}
      onClick={() => reply([option.id], null)}>{option.label}</button>)}
    {field.allowFreeText ? <form onSubmit={(event) => { event.preventDefault(); if (text && !disabled) reply([], text); }}>
      <input aria-label="终端回答" value={text} maxLength={1024} disabled={disabled}
        onChange={(event) => setText(event.target.value)} />
      <button type="submit" disabled={disabled || !text}>发送回答</button>
    </form> : null}
  </div>;
}
