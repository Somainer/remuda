import { useState, type KeyboardEvent } from "react";
import { Button } from "../../../components/Button";
import { composing } from "../../../lib/viewport";
import ui from "../../../styles/ui.module.css";

export function LocalInput({
  disabled,
  mobile,
  onSend,
}: {
  disabled: boolean;
  mobile: boolean;
  onSend: (text: string) => void;
}) {
  const [text, setText] = useState("");

  const submit = () => {
    if (!text || disabled) return;
    const value = text.endsWith("\n") ? text : `${text}\r`;
    setText("");
    onSend(value);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    if (!mobile && event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submit();
    }
  };

  return (
    <form
      className={ui.card}
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
    >
      <textarea
        className={ui.textarea}
        value={text}
        disabled={disabled}
        placeholder="本地输入：直连关闭时击键进 textarea"
        aria-label="本地输入"
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <div className={ui.row} style={{ marginTop: 8, justifyContent: "flex-end" }}>
        <Button variant="primary" disabled={disabled || !text} onClick={submit}>
          发送
        </Button>
      </div>
    </form>
  );
}
