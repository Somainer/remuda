import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { PERMISSION_OPTIONS } from "../../lib/sessionOptions";
import { composing } from "../../lib/viewport";
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
  onSend: (text: string) => Promise<void> | void;
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
      <div className={css.composer}>
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
          onKeyDown={onKeyDown}
        />
        {mobile ? (
          <button
            type="button"
            className={css.sendIcon}
            data-testid="composer-send"
            aria-label="送出"
            disabled={disabled || sending || !text.trim()}
            onClick={() => void submit()}
          >
            ↑
          </button>
        ) : null}
      </div>
      <div className={css.controlBar} ref={barRef} data-testid="composer-bar">
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
          <button type="button" className={css.send} data-testid="composer-send" disabled={disabled || sending || !text.trim()} onClick={() => void submit()}>
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
