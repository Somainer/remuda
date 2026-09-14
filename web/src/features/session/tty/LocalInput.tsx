import { useState, type KeyboardEvent } from "react";
import { composing } from "../../../lib/viewport";
import css from "./TerminalView.module.css";

export function LocalInput({
  disabled,
  mobile,
  onSend,
}: {
  disabled: boolean;
  mobile: boolean;
  /**
   * Delivers the BODY only. D-028 §5.2: text and Enter must be separate
   * writes (body, quiet wait, then `\r`); the `instance.send` driver path
   * does that server-side. Appending `\r` here reproduced the "one write never
   * submits to the Claude TUI" failure on the mobile dock.
   */
  onSend: (text: string) => void;
}) {
  const [text, setText] = useState("");

  const submit = () => {
    if (!text || disabled) return;
    const value = text;
    setText("");
    onSend(value);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (composing(event)) return;
    if (!mobile && event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submit();
    }
  };

  return (
    <div className={css.localWrap}>
      <form
        className={css.localForm}
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <div className={css.localField}>
          <input
            value={text}
            disabled={disabled}
            placeholder="本地输入（走 instance.send：正文与回车分两次写）"
            aria-label="本地输入"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </div>
        <button type="submit" className={css.localSend} disabled={disabled || !text} aria-label="发送">
          {mobile ? "↑" : "发送"}
        </button>
      </form>
      {mobile ? <p className={css.localHint}>本地输入（默认）· 中文候选不会串进 PTY · 不再 text+回车一次写</p> : null}
    </div>
  );
}
