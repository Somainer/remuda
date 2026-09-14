import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { PERMISSION_OPTIONS } from "../../lib/sessionOptions";
import { composing } from "../../lib/viewport";
import type { PromptMode } from "../../types/generated";
import type { CapabilitySnapshot } from "../../types/nativeRef";
import { AttachButtons, AttachmentChips } from "./AttachmentChips";
import { EffortSlider } from "./EffortSlider";
import {
  defaultEffortIndex,
  effortAt,
  effortCaps,
  effortStopName,
  effortTable,
  effortWireName,
  harnessMeta,
  isEmberEffort,
  mapEffort,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import { composerState, type Phase } from "../composer/state";
import {
  effectiveLabel,
  effortMismatch,
  isEffortUnknown,
  type EffortEffectiveView,
} from "./effortEffective";
import { useAttachments } from "./useAttachments";
import type { AttachmentRef, Attachment } from "../../lib/attachments";
import css from "./session.module.css";

type MenuId = "effort" | "permission" | null;
type Placement = "up" | "down";

type HeldItem = {
  id: string;
  text: string;
  holder: "remuda" | "native";
};

let heldSeq = 0;

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  onInterrupt,
  permissionMode = "manual",
  onPermission,
  kind = "claude",
  model = "opus",
  models,
  effort,
  onEffort,
  effortEffective,
  onModel,
  contextLabel,
  effortDisabled,
  phase = "idle",
  capabilities,
}: {
  instanceId: string;
  mobile: boolean;
  disabled?: boolean;
  sending?: boolean;
  onSend: (
    text: string,
    attachments?: AttachmentRef[],
    staged?: Attachment[],
    mode?: PromptMode,
  ) => Promise<void> | void;
  /** D-028 §5.3 instance.cancel — interrupt the turn, process stays alive. */
  onInterrupt?: () => void | Promise<void>;
  permissionMode?: string;
  onPermission?: (mode: string) => void;
  kind?: EffortKind | string;
  model?: string;
  models?: string[];
  effort?: EffortSelection;
  onEffort?: (next: EffortSelection) => void;
  /** §9.1 transcript-read-back level; null/undefined = unobserved (`?`). */
  effortEffective?: EffortEffectiveView | null;
  onModel?: (model: string) => void;
  contextLabel?: string | null;
  /** True when the session cannot take instance.configure (exited / observed-only). */
  effortDisabled?: boolean;
  /** Projected instance phase driving the §6 three-state controls. */
  phase?: Phase;
  /** Live capability snapshot; defaults keep idle sends working in fixtures. */
  capabilities?: CapabilitySnapshot;
}) {
  const [text, setText] = useState(() => readDraft(instanceId));
  const [menu, setMenu] = useState<MenuId>(null);
  const [placement, setPlacement] = useState<Placement>("down");
  const [held, setHeld] = useState<HeldItem[]>([]);
  const [interrupted, setInterrupted] = useState(false);
  const rootRef = useRef<HTMLFormElement>(null);
  const images = useAttachments(instanceId);
  const barRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const phaseRef = useRef(phase);
  // The harness is fixed for the life of a session; it is chosen on New Session.
  const harness = kind;

  const caps = effortCaps(harness);
  const incoming = effort ?? effortAt((kind as EffortKind) || "claude", defaultEffortIndex(harness));
  const currentEffort =
    incoming.kind === harness ? incoming : mapEffort(incoming, (harness as EffortKind) || "claude");
  const table = effortTable(harness);
  const ultraOn = currentEffort.ultracode === true;
  const ember = isEmberEffort(harness, currentEffort.index, ultraOn);
  const effortLocked = Boolean(effortDisabled) || !onEffort || table.length === 0;
  const permLabel = PERMISSION_OPTIONS.find((m) => m.id === permissionMode)?.label ?? permissionMode;

  const controls = composerState(
    harness,
    phase,
    capabilities ?? ({ capabilities: {} } as unknown as CapabilitySnapshot),
  );

  const busy = phase === "working" || phase === "blocked";
  const remudaHeld = held.filter((item) => item.holder === "remuda");

  // D-028 §6: Remuda-held items are delivered when the turn ends. Native-held
  // chips are ledger mirrors only and flush themselves.
  useEffect(() => {
    const wasBusy = phaseRef.current === "working";
    phaseRef.current = phase;
    if (!wasBusy || phase !== "idle") return;
    if (remudaHeld.length === 0) return;
    const items = remudaHeld;
    setHeld((cur) => cur.filter((item) => item.holder !== "remuda"));
    void (async () => {
      for (const item of items) {
        await onSend(item.text, undefined, undefined, "new-turn");
      }
    })();
    setInterrupted(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phase]);

  // The interrupted badge survives until the next idle turn (or 4 s, so a
  // cancel whose idle signal is missed still clears).
  useEffect(() => {
    if (!interrupted) return;
    const timer = window.setTimeout(() => setInterrupted(false), 4000);
    return () => window.clearTimeout(timer);
  }, [interrupted]);

  const canSubmit = () => {
    const value = text.trim();
    if ((!value && images.attachments.length === 0) || disabled || sending) return false;
    if (images.uploading) return false;
    return true;
  };

  const clearBox = () => {
    writeDraft(instanceId, "");
    setText("");
    images.handOff();
  };

  const doInterrupt = async () => {
    if (!controls.interrupt.available) return;
    await onInterrupt?.();
    setInterrupted(true);
  };

  /** Submit the PRIMARY control: send/steer goes out, queue holds a chip. */
  const submitPrimary = async () => {
    if (!canSubmit()) return;
    const value = text.trim();
    const refs = images.refs();
    const staged = images.attachments;
    const action = controls.primary;
    if (action.kind === "queue") {
      // Primary queue (no native send-now): Remuda holds it until idle.
      setHeld((cur) => [
        ...cur,
        { id: `held_${++heldSeq}`, text: value, holder: "remuda" },
      ]);
      clearBox();
      return;
    }
    clearBox();
    await onSend(value, refs, staged, action.mode);
  };

  /** Explicit secondary queue control: native Tab sends now; otherwise hold. */
  const submitQueue = () => {
    if (!controls.queue.available || !canSubmit()) return;
    const value = text.trim();
    const refs = images.refs();
    const staged = images.attachments;
    if (controls.queue.holder === "native") {
      setHeld((cur) => [
        ...cur,
        { id: `held_${++heldSeq}`, text: value, holder: "native" },
      ]);
      clearBox();
      void onSend(value, refs, staged, "queue");
    } else {
      setHeld((cur) => [
        ...cur,
        { id: `held_${++heldSeq}`, text: value, holder: "remuda" },
      ]);
      clearBox();
    }
  };

  /** Emulated/unknown send-now: confirm, cancel the turn, then steer. */
  const submitInterruptAndSend = async () => {
    if (!canSubmit() || !controls.interruptAndSend) return;
    const confirmed = window.confirm(
      controls.note
        ? `${controls.note}。确定打断当前 turn 并立即发送吗？`
        : "打断当前 turn 并立即发送？",
    );
    if (!confirmed) return;
    const value = text.trim();
    const refs = images.refs();
    const staged = images.attachments;
    await doInterrupt();
    clearBox();
    await onSend(value, refs, staged, "steer");
  };

  const removeHeld = (id: string) => {
    setHeld((cur) => cur.filter((item) => item.id !== id));
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    if (mobile) return;
    // Esc while the composer is focused = 打断, with a confirm (desktop only).
    if (event.key === "Escape" && busy && controls.interrupt.available) {
      event.preventDefault();
      event.stopPropagation();
      const confirmed = window.confirm("打断当前 turn？会话与进程不会退出。");
      if (confirmed) void doInterrupt();
      return;
    }
    // Enter = primary (Cmd/Ctrl+Enter is the same primary, kept for muscle
    // memory); Shift+Enter falls through to the textarea's newline.
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submitPrimary();
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
  // The chip names the current stop ("ultracode" at the top stop); the form's
  // data-attr carries the wire name so an ultracode selection round-trips.
  const effortChipLabel = effortStopName(harness, currentEffort.name, ultraOn);
  const effortWire = effortWireName(currentEffort);
  // §9.1: the chip text is the EFFECTIVE level read back from the transcript,
  // not the requested selection. `?` until the first assistant record; a
  // requested/effective divergence renders explicitly, it is never hidden.
  const effectiveUnknown = isEffortUnknown(effortEffective);
  const effectiveWord = effectiveLabel(effortEffective);
  const mismatch = caps.effort
    ? effortMismatch(effortWire, ultraOn, effortEffective)
    : null;
  const effortChipTitle = effectiveUnknown
    ? "实际档位：等待会话回读（？）"
    : mismatch
      ? `请求 ${mismatch.requested} → 实际 ${mismatch.effective}`
      : `实际档位 ${effectiveWord}（来源 ${effortEffective?.source ?? "unknown"}）`;
  const primaryLabel = sending
    ? "发送中"
    : controls.primary.kind === "steer"
      ? "发送"
      : controls.primary.label;
  const primaryTestId =
    controls.primary.kind === "queue" ? "composer-queue" : "composer-send";

  return (
    <form
      ref={rootRef}
      data-testid="composer"
      data-harness={harness}
      data-phase={phase}
      data-effort={effortWire}
      data-effort-index={String(currentEffort.index)}
      data-ultracode={ultraOn ? "1" : "0"}
      data-model={model}
      className={css.composerRoot}
      onSubmit={(e) => {
        e.preventDefault();
        void submitPrimary();
      }}
    >
      {held.length ? (
        <div className={css.queuedRow} data-testid="composer-queued-row">
          {held.map((item) => (
            <span
              key={item.id}
              className={css.queuedChip}
              data-testid="composer-queued-chip"
              data-holder={item.holder}
              title={item.holder === "remuda" ? "Remuda 代持：turn 结束后投递，可撤回" : "harness 原生队列"}
            >
              <span className={css.queuedTag}>{item.holder === "remuda" ? "排队" : "原生"}</span>
              <span className={css.queuedText}>{item.text}</span>
              {item.holder === "remuda" ? (
                <button
                  type="button"
                  className={css.queuedRemove}
                  data-testid="composer-queued-remove"
                  aria-label="撤回排队消息"
                  onClick={() => removeHeld(item.id)}
                >
                  ✕
                </button>
              ) : null}
            </span>
          ))}
        </div>
      ) : null}
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
          placeholder="输入提示词…  Enter 主操作 · Shift+Enter 换行 · 工作中 Esc 打断 · IME 组字期间不送"
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
            data-effort-effective={effectiveUnknown ? "unknown" : effectiveWord}
            data-effort-source={effortEffective?.source ?? "unknown"}
            data-effort-mismatch={mismatch ? "1" : "0"}
            aria-expanded={menu === "effort"}
            aria-haspopup="dialog"
            aria-label={`Select effort, ${effortChipLabel}; effective ${effectiveUnknown ? "unknown" : effectiveWord}`}
            title={effortChipTitle}
            onClick={() => toggle("effort")}
          >
            {ember ? (
              <>
                <span className={css.emberSpark} />
                <span className={`${css.emberSpark} ${css.emberSpark2}`} />
                <span className={`${css.emberSpark} ${css.emberSpark3}`} />
            </>
            ) : null}
            <span className={css.chipModel} data-testid="model-effort-chip-label">
              {effectiveUnknown ? "?" : effectiveWord}
            </span>
            {mismatch ? (
              <span className={css.chipEffortMismatch} data-testid="model-effort-mismatch">
                请求 {mismatch.requested} → 实际 {mismatch.effective}
              </span>
            ) : null}
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
        {held.length ? (
          <span className={css.chip} data-testid="composer-queue-status">
            已排队 {held.length}
          </span>
        ) : null}
        {interrupted ? (
          <span className={css.chip} data-testid="composer-interrupted-chip">
            已打断
          </span>
        ) : null}
        {controls.note ? (
          <span className={css.controlNote} data-testid="composer-cap-note" title={controls.note}>
            {controls.note}
          </span>
        ) : null}
        <span className={css.barSpacer} />
        {/* D-028 §6 three-state controls */}
        {busy && controls.queue.available ? (
          <button
            type="button"
            className={css.queueBtn}
            data-testid="composer-queue-btn"
            data-holder={controls.queue.holder}
            disabled={disabled || sending || images.uploading || !text.trim()}
            title={controls.queue.holder === "remuda" ? "Remuda 代持，turn 结束后投递" : "harness 原生排队（Tab）"}
            onClick={() => submitQueue()}
          >
            排队
            {controls.queue.holder === "remuda" ? <span className={css.controlSub}>Remuda 代持</span> : null}
          </button>
        ) : null}
        {busy && controls.interruptAndSend ? (
          <button
            type="button"
            className={css.queueBtn}
            data-testid="composer-interrupt-send"
            disabled={disabled || sending || images.uploading || !text.trim()}
            onClick={() => void submitInterruptAndSend()}
          >
            打断并发送
          </button>
        ) : null}
        {busy && controls.interrupt.available ? (
          <button
            type="button"
            className={css.interruptBtn}
            data-testid="composer-interrupt"
            data-provision={controls.interrupt.provision}
            disabled={disabled}
            title={controls.interrupt.note ?? "打断当前 turn（会话与进程不退出）"}
            onClick={() => void doInterrupt()}
          >
            打断
            {controls.interrupt.note ? <span className={css.controlSub}>{controls.interrupt.note}</span> : null}
          </button>
        ) : null}
        <button
          type="submit"
          className={controls.primary.kind === "queue" ? css.queueBtn : css.send}
          data-testid={primaryTestId}
          data-mode={controls.primary.mode}
          disabled={disabled || sending || images.uploading || (!text.trim() && images.attachments.length === 0)}
        >
          {primaryLabel}
        </button>
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
            ultracode={ultraOn}
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
