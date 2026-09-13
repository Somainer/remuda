import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { PERMISSION_OPTIONS } from "../../lib/sessionOptions";
import { composing } from "../../lib/viewport";
import { AttachButtons, AttachmentChips } from "./AttachmentChips";
import { EffortSlider } from "./EffortSlider";
import {
  effortCaps,
  effortTable,
  harnessMeta,
  isEmberTier,
  mapEffort,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import { useAttachments } from "./useAttachments";
import type { AttachmentRef, Attachment } from "../../lib/attachments";
import css from "./session.module.css";

type MenuId = "effort" | "permission" | null;
type Placement = "up" | "down";

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  permissionMode = "manual",
  onPermission,
  kind = "claude",
  model = "opus",
  models,
  effort,
  onEffort,
  onModel,
  contextLabel,
  effortDisabled,
}: {
  instanceId: string;
  mobile: boolean;
  disabled?: boolean;
  sending?: boolean;
  onSend: (text: string, attachments?: AttachmentRef[], staged?: Attachment[]) => Promise<void> | void;
  permissionMode?: string;
  onPermission?: (mode: string) => void;
  kind?: EffortKind | string;
  model?: string;
  models?: string[];
  effort?: EffortSelection;
  onEffort?: (next: EffortSelection) => void;
  onModel?: (model: string) => void;
  contextLabel?: string | null;
  /** True when the session cannot take instance.configure (exited / observed-only). */
  effortDisabled?: boolean;
}) {
  const [text, setText] = useState(() => readDraft(instanceId));
  const [menu, setMenu] = useState<MenuId>(null);
  const [placement, setPlacement] = useState<Placement>("down");
  const rootRef = useRef<HTMLFormElement>(null);
  const images = useAttachments(instanceId);
  const barRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  // The harness is fixed for the life of a session; it is chosen on New Session.
  const harness = kind;

  const caps = effortCaps(harness);
  const incoming = effort ?? { index: 1, name: "think", kind: (kind as EffortKind) || "claude" };
  const currentEffort =
    incoming.kind === harness ? incoming : mapEffort(incoming, (harness as EffortKind) || "claude");
  const table = effortTable(harness);
  const ember = isEmberTier(harness, currentEffort.index);
  const effortLocked = Boolean(effortDisabled) || !onEffort || table.length === 0;
  const permLabel = PERMISSION_OPTIONS.find((m) => m.id === permissionMode)?.label ?? permissionMode;

  const submit = async () => {
    const value = text.trim();
    // An image on its own is a legitimate message, so an empty box is only a
    // blocker when there is nothing attached either.
    if ((!value && images.attachments.length === 0) || disabled || sending) return;
    // Never send while an upload is still in flight: the reference would not
    // resolve on the Hub yet.
    if (images.uploading) return;
    const refs = images.refs();
    const staged = images.attachments;
    writeDraft(instanceId, "");
    setText("");
    // Hand the chips to the sent bubble, which takes over their preview URLs.
    images.handOff();
    await onSend(value, refs, staged);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    if (mobile) return;
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void submit();
    }
  };

  useEffect(() => {
    if (!menu) return;
    const onDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setMenu(null);
    };
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") setMenu(null);
    };
    window.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  useLayoutEffect(() => {
    if (!menu) return;
    const bar = barRef.current;
    const panel = menuRef.current;
    if (!bar || !panel) return;
    const barRect = bar.getBoundingClientRect();
    const approval = document.querySelector("[data-testid='approval-card'], [data-testid='question-form']");
    const approvalBottom = approval?.getBoundingClientRect().bottom ?? 0;
    const roomAbove = barRect.top - Math.max(approvalBottom, 0);
    const next = roomAbove >= panel.offsetHeight + 8 ? "up" : "down";
    if (next !== placement) setPlacement(next);
  }, [menu, harness, currentEffort.index, placement]);

  const toggle = (id: MenuId) => {
    setPlacement("down");
    setMenu((cur) => (cur === id ? null : id));
  };

  const harnessChip = harnessMeta(harness);
  const effortChipLabel = currentEffort.name;

  return (
    <form
      ref={rootRef}
      data-testid="composer"
      data-harness={harness}
      data-effort={currentEffort.name}
      data-effort-index={String(currentEffort.index)}
      data-model={model}
      className={css.composerRoot}
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <AttachmentChips
        attachments={images.attachments}
        onRemove={images.remove}
        onRetry={images.retry}
      />
      {images.notice ? (
        <div className={css.attachNotice} data-testid="attachment-notice">
          {images.notice}
        </div>
      ) : null}
      <div
        className={css.composer}
        onDragOver={(event) => {
          if (Array.from(event.dataTransfer.types).includes("Files")) event.preventDefault();
        }}
        onDrop={(event) => {
          const dropped = Array.from(event.dataTransfer.files).filter((file) =>
            file.type.startsWith("image/"),
          );
          if (dropped.length === 0) return;
          event.preventDefault();
          images.add(dropped);
        }}
      >
        <textarea
          className={css.input}
          data-testid="composer-input"
          value={text}
          disabled={disabled}
          placeholder={mobile ? "输入提示词…" : "输入提示词…  Enter 送出 · Shift+Enter 换行 · IME 组字期间不送"}
          onChange={(e) => {
            setText(e.target.value);
            writeDraft(instanceId, e.target.value);
          }}
          onPaste={(event) => {
            // Only swallow the paste when an image was actually taken:
            // otherwise plain-text pasting and the iOS caret both break.
            if (images.onPaste(event.clipboardData)) event.preventDefault();
          }}
          onKeyDown={onKeyDown}
        />
        {mobile ? (
          <button
            type="button"
            className={css.sendIcon}
            data-testid="composer-send"
            aria-label="送出"
            disabled={disabled || sending || images.uploading || (!text.trim() && images.attachments.length === 0)}
            onClick={() => void submit()}
          >
            ↑
          </button>
        ) : null}
      </div>
      <div className={css.controlBar} ref={barRef} data-testid="composer-bar">
        <AttachButtons
          className={css.chip}
          disabled={disabled}
          mobile={mobile}
          onFiles={images.add}
          onPasteClick={() => void images.pasteFromClipboard()}
        />
        {caps.harness ? (
          <span className={css.chip} data-testid="harness-chip" data-readonly="1">
            <span className={css.chipMark}>{harnessChip.mark}</span>
            <span>{mobile ? harnessChip.label.replace(" Code", "") : harnessChip.label}</span>
          </span>
        ) : null}
        {caps.effort ? (
          <button
            type="button"
            className={`${css.chip} ${ember ? css.ember : ""}`}
            data-testid="model-effort-chip"
            data-ember={ember ? "1" : "0"}
            aria-expanded={menu === "effort"}
            aria-haspopup="dialog"
            aria-label={`Select effort, ${effortChipLabel}`}
            onClick={() => toggle("effort")}
          >
            {ember ? (
              <>
                <span className={css.emberSpark} />
                <span className={`${css.emberSpark} ${css.emberSpark2}`} />
                <span className={`${css.emberSpark} ${css.emberSpark3}`} />
              </>
            ) : null}
            <span className={css.chipModel}>{effortChipLabel}</span>
            <span className={css.chipCaret}>▾</span>
          </button>
        ) : null}
        {caps.context ? (
          <span className={css.chip} data-testid="context-chip">
            <span
              className={css.contextRing}
              style={{ ["--ctx-pct" as string]: contextLabel?.endsWith("%") ? contextLabel : "0%" }}
            />
            <span>{contextLabel ?? "—"}</span>
          </span>
        ) : null}
        {caps.permission ? (
          onPermission ? (
            <button
              type="button"
              className={css.chip}
              data-testid="permission-chip"
              aria-expanded={menu === "permission"}
              onClick={() => toggle("permission")}
            >
              {mobile ? permLabel : `权限 ${permLabel}`} ▾
            </button>
          ) : (
            <span className={css.chip} data-testid="permission-chip" data-readonly="1">
              {permLabel}
            </span>
          )
        ) : null}
        <span className={css.barSpacer} />
        {mobile ? null : (
          <button type="button" className={css.send} data-testid="composer-send" disabled={disabled || sending || images.uploading || (!text.trim() && images.attachments.length === 0)} onClick={() => void submit()}>
            {sending ? "发送中" : "送出"}
          </button>
        )}
      </div>
      {menu === "effort" ? (
        <div
          ref={menuRef}
          className={`${css.popover} ${css.popoverCard} ${placement === "up" ? css.popoverUp : ""}`}
          data-testid="effort-menu"
          data-placement={placement}
        >
          <EffortSlider
            kind={harness}
            model={caps.model ? model : undefined}
            models={caps.model ? models : undefined}
            index={currentEffort.index}
            disabled={effortLocked}
            onChange={(next) => onEffort?.(next)}
            onModel={caps.model ? onModel : undefined}
          />
        </div>
      ) : null}
      {menu === "permission" ? (
        <div
          ref={menuRef}
          className={`${css.popover} ${placement === "up" ? css.popoverUp : ""}`}
          data-testid="permission-menu"
          data-placement={placement}
        >
          {PERMISSION_OPTIONS.map((m) => (
            <button
              key={m.id}
              type="button"
              className={`${css.effortRow} ${permissionMode === m.id ? css.effortOn : ""}`}
              data-testid={`permission-option-${m.id}`}
              onClick={() => {
                onPermission?.(m.id);
                setMenu(null);
              }}
            >
              <span className={`${css.radio} ${permissionMode === m.id ? css.radioOn : ""}`} />
              <span className={css.effortName}>{m.label}</span>
              <span className={css.effortDesc}>{m.id}</span>
            </button>
          ))}
        </div>
      ) : null}
    </form>
  );
}
