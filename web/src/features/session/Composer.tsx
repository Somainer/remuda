import { useState, type KeyboardEvent } from "react";
import { Button } from "../../components/Button";
import { readDraft, writeDraft } from "../../lib/drafts";
import { composing } from "../../lib/viewport";
import ui from "../../styles/ui.module.css";

const MODES = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "dontAsk", label: "全自动" },
];

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  permissionMode = "manual",
  onPermission,
}: {
  instanceId: string;
  mobile: boolean;
  disabled?: boolean;
  sending?: boolean;
  onSend: (text: string) => Promise<void> | void;
  permissionMode?: string;
  onPermission?: (mode: string) => void;
}) {
  const [text, setText] = useState(() => readDraft(instanceId));
  const [permOpen, setPermOpen] = useState(false);

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

  const permLabel = MODES.find((m) => m.id === permissionMode)?.label ?? permissionMode;

  return (
    <form
      className={ui.card}
      data-testid="composer"
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
        <div>
          <button type="button" className={ui.chip} data-testid="permission-chip" onClick={() => setPermOpen(!permOpen)}>
            权限:{permLabel}
          </button>
          {permOpen
            ? MODES.map((m) => (
                <button
                  key={m.id}
                  type="button"
                  className={`${ui.chip} ${permissionMode === m.id ? ui.chipOn : ""}`}
                  onClick={() => {
                    onPermission?.(m.id);
                    setPermOpen(false);
                  }}
                >
                  {m.label}
                </button>
              ))
            : null}
        </div>
        <Button variant="primary" disabled={disabled || sending || !text.trim()} onClick={() => void submit()}>
          {sending ? "发送中" : "送出"}
        </Button>
      </div>
    </form>
  );
}
