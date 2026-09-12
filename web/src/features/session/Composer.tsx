import { useState, type KeyboardEvent } from "react";
import { Button } from "../../components/Button";
import { readDraft, writeDraft } from "../../lib/drafts";
import { composing } from "../../lib/viewport";
import ui from "../../styles/ui.module.css";

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  permissionMode,
}: {
  instanceId: string;
  mobile: boolean;
  disabled?: boolean;
  sending?: boolean;
  onSend: (text: string) => Promise<void> | void;
  permissionMode?: string;
}) {
  const [text, setText] = useState(() => readDraft(instanceId));

  const submit = async () => {
    const value = text.trim();
    if (!value || disabled || sending) return;
    writeDraft(instanceId, "");
    setText("");
    await onSend(value);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    if (!mobile && event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  };

  return (
    <form
      className={ui.card}
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <textarea
        className={ui.textarea}
        value={text}
        disabled={disabled}
        placeholder="输入提示词…"
        onChange={(e) => {
          setText(e.target.value);
          writeDraft(instanceId, e.target.value);
        }}
        onKeyDown={onKeyDown}
      />
      <div className={ui.row} style={{ marginTop: 8, justifyContent: "space-between" }}>
        <span className={ui.listMeta}>权限:{permissionMode ?? "询问"}</span>
        <Button variant="primary" disabled={disabled || sending || !text.trim()} onClick={() => void submit()}>
          {sending ? "发送中" : "送出"}
        </Button>
      </div>
    </form>
  );
}
