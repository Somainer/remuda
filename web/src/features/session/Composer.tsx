import { useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { PERMISSION_OPTIONS } from "../../lib/sessionOptions";
import { composing } from "../../lib/viewport";
import css from "./session.module.css";

const MODES = PERMISSION_OPTIONS;

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
    if (mobile) return;
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void submit();
    }
  };

  const permLabel = MODES.find((m) => m.id === permissionMode)?.label ?? permissionMode;

  return (
    <form
      data-testid="composer"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <div className={css.composer}>
        <textarea
          className={css.input}
          value={text}
          disabled={disabled}
          placeholder={mobile ? "输入提示词…" : "输入提示词…  Enter 送出 · Shift+Enter 换行 · IME 组字期间不送"}
          onChange={(e) => {
            setText(e.target.value);
            writeDraft(instanceId, e.target.value);
          }}
          onKeyDown={onKeyDown}
        />
        {mobile ? (
          <button
            type="button"
            className={css.sendIcon}
            aria-label="送出"
            disabled={disabled || sending || !text.trim()}
            onClick={() => void submit()}
          >
            ↑
          </button>
        ) : (
          <button type="button" className={css.perm} data-testid="permission-chip" onClick={() => setPermOpen(!permOpen)}>
            权限 {permLabel} ▾
          </button>
        )}
        {mobile ? null : (
          <button type="button" className={css.send} disabled={disabled || sending || !text.trim()} onClick={() => void submit()}>
            {sending ? "发送中" : "送出"}
          </button>
        )}
      </div>
      {mobile ? (
        <div className={css.permMenu} style={{ marginTop: 10 }}>
          <button type="button" className={css.perm} data-testid="permission-chip" onClick={() => setPermOpen(!permOpen)}>
            权限 {permLabel} ▾
          </button>
        </div>
      ) : null}
      {permOpen ? (
        <div className={css.permMenu} style={{ marginTop: 8 }}>
          {MODES.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`${css.qOpt} ${permissionMode === m.id ? css.qOptOn : ""}`}
              onClick={() => {
                onPermission?.(m.id);
                setPermOpen(false);
              }}
            >
              {m.label}
            </button>
          ))}
        </div>
      ) : null}
    </form>
  );
}
