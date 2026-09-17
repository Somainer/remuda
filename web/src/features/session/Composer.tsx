import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { readDraft, writeDraft } from "../../lib/drafts";
import { expandCodeQuotes } from "../../lib/codeAnchors";
import {
  insertAnchorFor,
  insertAttachmentAnchors,
  referencedAttachmentIndices,
  referencedIndicesFor,
  removeAndRenumberAttachment,
  removeAndRenumberFor,
  type AttachmentAnchor,
} from "../../lib/imageAnchors";
import {
  isLiveReachable,
  launchPermissionTable,
  normalizePermissionMode,
} from "./permissions";
import type { PermissionEffectiveView } from "./permissionEffective";
import { composing } from "../../lib/viewport";
import type { PromptMode } from "../../types/generated";
import type { CapabilitySnapshot } from "../../types/nativeRef";
import { AttachButtons, AttachmentChips, CodeQuoteChips } from "./AttachmentChips";
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
import { useCodeQuotes } from "./useCodeQuotes";
import { ContextUsagePopover } from "./ContextUsagePopover";
import type { UsageRollup } from "./contextUsage";
import type { AttachmentRef, Attachment } from "../../lib/attachments";
import css from "./session.module.css";

type MenuId = "effort" | "permission" | "usage" | null;
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
  launchPermissionMode,
  permissionEffective = null,
  permissionPending = null,
  kind = "claude",
  model = "opus",
  models,
  modelEffective,
  modelPending,
  effort,
  onEffort,
  effortEffective,
  effortPending,
  onModel,
  contextLabel,
  usageRollup,
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
  /** Mode the session launched with; decides whether bypass is live-reachable. */
  launchPermissionMode?: string;
  /** Read-back effective mode; the chip renders from this. */
  permissionEffective?: PermissionEffectiveView | null;
  /** A wheel walk in flight (chip shows 切换中 / 排队中 until read-back). */
  permissionPending?: { mode: string; queued: boolean } | null;
  kind?: EffortKind | string;
  model?: string;
  models?: string[];
  /** §9.1 transcript-read-back effective model id; null/undefined = unobserved. */
  modelEffective?: string | null;
  /** §9.1 a model switch in flight. */
  modelPending?: { id: string; queued: boolean } | null;
  effort?: EffortSelection;
  onEffort?: (next: EffortSelection) => void;
  /** §9.1 transcript-read-back level; null/undefined = unobserved (`?`). */
  effortEffective?: EffortEffectiveView | null;
  /** §9.1 a push-down in flight (chip shows 切换中 / 排队中 until read-back). */
  effortPending?: { word: string; queued: boolean } | null;
  onModel?: (model: string) => void;
  contextLabel?: string | null;
  /** context-usage-1: Hub-computed per-session token/context rollup; the chip
   *  ring and its hover popover read this. The legacy contextLabel prop is
   *  the fallback (mock harness / older Hub). */
  usageRollup?: UsageRollup | null;
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
  const hoverCloseTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // A click (rather than a hover) pins the panel open: subsequent pointer
  // leaves must not dismiss it. Reset on every real dismiss path.
  const usagePinned = useRef(false);
  const rootRef = useRef<HTMLFormElement>(null);
  const images = useAttachments(instanceId);
  const codeQuotes = useCodeQuotes((index) => insertCodeTokenRef.current?.(index));
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Synchronous mirror so rapid pastes and the drop handler see the text the
  // previous insert just produced rather than a stale React closure.
  const textRef = useRef(text);
  textRef.current = text;
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
  // Permission: the chip renders the read-back mode; pending overrides it
  // with 切换中/排队中. The menu lists the harness's real launch table, with
  // launch-only rows greyed for the live session.
  const permOptions = launchPermissionTable(harness);
  const liveMode = normalizePermissionMode(
    harness,
    permissionPending?.mode ?? permissionEffective?.mode ?? permissionMode,
  );
  const permOption =
    permOptions.find((m) => m.id === liveMode) ??
    permOptions.find((m) => m.id === permissionMode);
  // Read-only chips (generic-pty / other harnesses) render the native word;
  // interactive chips render the localized label from the harness table.
  const permLabel = onPermission ? permOption?.label ?? liveMode : permissionMode;
  const permTag = permissionPending
    ? permissionPending.queued
      ? "排队中"
      : "切换中"
    : null;

  // context-usage-1: when a Hub rollup exists it is authoritative, including
  // its explicit null (an output-only Grok turn means context is UNKNOWN —
  // never fall through to the client-side last-event estimate and paint 0%).
  // The legacy contextLabel prop only drives mock-harness sessions with no
  // rollup channel.
  const contextPct = usageRollup
    ? (usageRollup.contextPct ?? null)
    : contextLabel?.endsWith("%")
      ? Number(contextLabel.slice(0, -1))
      : null;
  const contextChipLabel = contextPct == null || !Number.isFinite(contextPct) ? "—" : `${contextPct}%`;
  const hoverCapable = () =>
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(hover: hover) and (pointer: fine)").matches;
  // The popover is not a DOM child of the chip, so the cursor crossing the
  // gap between them must not close the card: leave schedules a short close
  // that entering the popover cancels.
  const cancelHoverClose = () => {
    if (hoverCloseTimer.current != null) {
      clearTimeout(hoverCloseTimer.current);
      hoverCloseTimer.current = null;
    }
  };
  const scheduleHoverClose = () => {
    cancelHoverClose();
    if (!hoverCapable() || usagePinned.current) return;
    hoverCloseTimer.current = setTimeout(() => setMenu(null), 140);
  };
  const dismissUsage = () => {
    usagePinned.current = false;
    cancelHoverClose();
    setMenu(null);
  };
  useEffect(() => () => cancelHoverClose(), []);

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
    const value = expandCodeQuotes(text.trim(), codeQuotes.quotes);
    if ((!value && images.attachments.length === 0) || disabled || sending) return false;
    if (images.uploading) return false;
    return true;
  };

  /**
   * Insert `[Image #n]`/`[Code #n]` token at the textarea caret
   * (mid-word inserts get surrounding spaces — see imageAnchors). Focus moves
   * to the composer after insert; on touch-sized layouts the composer is
   * scrolled into view so the chip/token is visible above the keyboard.
   */
  const insertTokenAtCaret = (kind: "Image" | "Code", index: number) => {
    const area = inputRef.current;
    const caret = area && area.selectionStart != null ? area.selectionStart : textRef.current.length;
    const result = insertAnchorFor(kind, textRef.current, caret, index);
    textRef.current = result.text;
    setText(result.text);
    writeDraft(instanceId, result.text);
    requestAnimationFrame(() => {
      const next = inputRef.current;
      if (!next) return;
      next.focus();
      next.setSelectionRange(result.caret, result.caret);
      if (mobile) next.scrollIntoView({ block: "center", behavior: "smooth" });
    });
  };

  /**
   * Insert `[Image #n]`/`[File #n]` tokens for freshly staged files at the
   * textarea caret. A multi-file paste chains the caret so tokens land in
   * file order; images and files share one numbering space.
   */
  const insertForAttachments = (anchors: AttachmentAnchor[]) => {
    if (anchors.length === 0) return;
    const area = inputRef.current;
    const caret = area && area.selectionStart != null ? area.selectionStart : textRef.current.length;
    const result = insertAttachmentAnchors(textRef.current, caret, anchors);
    textRef.current = result.text;
    setText(result.text);
    writeDraft(instanceId, result.text);
    requestAnimationFrame(() => {
      const next = inputRef.current;
      if (!next) return;
      next.focus();
      next.setSelectionRange(result.caret, result.caret);
      if (mobile) next.scrollIntoView({ block: "center", behavior: "smooth" });
    });
  };

  /** 评论 on a code block: the quote is registered, then its token goes in. */
  const insertCodeToken = (index: number) => insertTokenAtCaret("Code", index);
  // The quote bus fires synchronously on click; the hook above reaches the
  // inserter through this ref (kept current every render).
  const insertCodeTokenRef = useRef(insertCodeToken);
  insertCodeTokenRef.current = insertCodeToken;

  /** Chip × : unstage the attachment, pull its token(s) out, renumber the rest. */
  const removeAttachment = (localId: string) => {
    const index = images.remove(localId);
    if (index === null) return;
    const next = removeAndRenumberAttachment(textRef.current, index);
    textRef.current = next;
    setText(next);
    writeDraft(instanceId, next);
  };

  /** Quote chip × : drop the quote and strip/renumber its [Code #n] token. */
  const removeCodeQuote = (index: number) => {
    codeQuotes.remove(index);
    const next = removeAndRenumberFor("Code", textRef.current, index);
    textRef.current = next;
    setText(next);
    writeDraft(instanceId, next);
  };

  /** Chips whose [Image #n]/[File #n] token was edited out; still sent. */
  const unreferenced = new Set(
    images.attachments
      .map((attachment, position) => ({ attachment, index: position + 1 }))
      .filter(({ index }) => !referencedAttachmentIndices(textRef.current).has(index))
      .map(({ attachment }) => attachment.localId),
  );

  /** Quote chips whose [Code #n] token was edited out; still sent. */
  const unreferencedCode = new Set(
    codeQuotes.quotes
      .map((quote, position) => ({ quote, index: position + 1 }))
      .filter(({ index }) => !referencedIndicesFor("Code", textRef.current).has(index))
      .map(({ quote }) => quote.localId),
  );

  const clearBox = () => {
    writeDraft(instanceId, "");
    setText("");
    images.handOff();
    codeQuotes.clear();
  };

  const doInterrupt = async () => {
    if (!controls.interrupt.available) return;
    await onInterrupt?.();
    setInterrupted(true);
  };

  /** Submit the PRIMARY control: send/steer goes out, queue holds a chip. */
  const submitPrimary = async () => {
    if (!canSubmit()) return;
    const value = expandCodeQuotes(text.trim(), codeQuotes.quotes);
    const refs = images.refs(value);
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
    const value = expandCodeQuotes(text.trim(), codeQuotes.quotes);
    const refs = images.refs(value);
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
    const value = expandCodeQuotes(text.trim(), codeQuotes.quotes);
    const refs = images.refs(value);
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
      if (!rootRef.current?.contains(event.target as Node)) dismissUsage();
    };
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") dismissUsage();
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
    if (id !== "usage") usagePinned.current = false;
    setPlacement("down");
    setMenu((cur) => (cur === id ? null : id));
  };

  const harnessChip = harnessMeta(harness);
  // The chip names the current stop ("ultracode" at the top stop); the form's
  // data-attr carries the wire name so an ultracode selection round-trips.
  const effortChipLabel = effortStopName(harness, currentEffort.name, ultraOn);
  const effortWire = effortWireName(currentEffort);
  // §9.1: the chip text is the EFFECTIVE level read back from the transcript,
  // not the requested selection. `?` until the first read-back of a fresh
  // session; a requested/effective divergence renders explicitly, it is never
  // hidden. While a push-down is in flight the chip shows the requested word
  // with a 切换中 / 排队中 tag instead of going ambiguous.
  const effectiveUnknown = isEffortUnknown(effortEffective);
  const pendingLabel = effortPending?.word ?? null;
  const effectiveWord = pendingLabel ?? effectiveLabel(effortEffective);
  const mismatch = caps.effort && !pendingLabel
    ? effortMismatch(effortWire, ultraOn, effortEffective)
    : null;
  const effortChipTitle = pendingLabel
    ? effortPending?.queued
      ? `排队中：${pendingLabel} 将在本回合结束后生效`
      : `切换中：${pendingLabel}`
    : effectiveUnknown
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
        unreferenced={unreferenced}
        onRemove={removeAttachment}
        onRetry={images.retry}
      />
      <CodeQuoteChips quotes={codeQuotes.quotes} unreferenced={unreferencedCode} onRemove={removeCodeQuote} />
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
          // Any file type, not only images (D-027b).
          const dropped = Array.from(event.dataTransfer.files);
          if (dropped.length === 0) return;
          event.preventDefault();
          insertForAttachments(images.add(dropped));
        }}
      >
        <textarea
          ref={inputRef}
          className={css.input}
          data-testid="composer-input"
          value={text}
          disabled={disabled}
          placeholder="输入提示词…  Enter 主操作 · Shift+Enter 换行 · 工作中 Esc 打断 · IME 组字期间不送"
          onChange={(e) => {
            textRef.current = e.target.value;
            setText(e.target.value);
            writeDraft(instanceId, e.target.value);
          }}
          onPaste={(event) => {
            // Swallow the paste only when a file was actually taken: otherwise
            // plain-text pasting and the iOS caret both break.
            const anchors = images.onPaste(event.clipboardData);
            if (anchors.length > 0) {
              event.preventDefault();
              insertForAttachments(anchors);
            }
          }}
          onKeyDown={onKeyDown}
        />
      </div>
      <div className={css.controlBar} ref={barRef} data-testid="composer-bar">
        <AttachButtons
          className={css.chip}
          disabled={disabled}
          mobile={mobile}
          onFiles={(files) => insertForAttachments(images.add(files))}
          onPasteClick={async () => insertForAttachments(await images.pasteFromClipboard())}
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
            className={`${css.chip} ${ember ? css.ember : ""} ${pendingLabel ? css.chipEffortPending : ""}`}
            data-testid="model-effort-chip"
            data-ember={ember ? "1" : "0"}
            data-effort-effective={pendingLabel ? "pending" : effectiveUnknown ? "unknown" : effectiveWord}
            data-effort-pending={pendingLabel ? (effortPending?.queued ? "queued" : "switching") : "0"}
            data-effort-source={effortEffective?.source ?? "unknown"}
            data-effort-mismatch={mismatch ? "1" : "0"}
            aria-expanded={menu === "effort"}
            aria-haspopup="dialog"
            aria-label={`Select effort, ${effortChipLabel}; effective ${pendingLabel ? `pending ${pendingLabel}` : effectiveUnknown ? "unknown" : effectiveWord}`}
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
              {pendingLabel ?? (effectiveUnknown ? "?" : effectiveWord)}
            </span>
            {pendingLabel ? (
              <span className={css.chipEffortPendingTag} data-testid="model-effort-pending">
                {effortPending?.queued ? "排队中" : "切换中"}
              </span>
            ) : null}
            {mismatch ? (
              <span className={css.chipEffortMismatch} data-testid="model-effort-mismatch">
                请求 {mismatch.requested} → 实际 {mismatch.effective}
              </span>
            ) : null}
            <span className={css.chipCaret}>▾</span>
          </button>
        ) : null}
        {caps.context ? (
          <button
            type="button"
            className={css.chip}
            data-testid="context-chip"
            data-has-popover={usageRollup ? "1" : "0"}
            aria-haspopup={usageRollup ? "dialog" : undefined}
            aria-expanded={menu === "usage"}
            aria-label={
              usageRollup
                ? `上下文用量 ${contextChipLabel}，查看明细`
                : `上下文用量 ${contextChipLabel}`
            }
            onClick={() => {
              // Idempotent, click-pinned open: mouseenter may already have
              // opened it on precise pointers, and touch fires no hover.
              // Pinning means the later pointer leave cannot dismiss the
              // card; × / outside pointerdown / Escape unpin and close.
              if (usageRollup) {
                usagePinned.current = true;
                cancelHoverClose();
                setPlacement("down");
                setMenu("usage");
              }
            }}
            onMouseEnter={() => {
              if (usageRollup && hoverCapable()) {
                cancelHoverClose();
                setPlacement("down");
                setMenu("usage");
              }
            }}
            onMouseLeave={scheduleHoverClose}
          >
            <span
              className={css.contextRing}
              style={{ ["--ctx-pct" as string]: contextPct == null ? "0%" : `${contextPct}%` }}
            />
            <span>{contextChipLabel}</span>
          </button>
        ) : null}
        {caps.permission ? (
          onPermission ? (
            <button
              type="button"
              className={`${css.chip} ${permissionPending ? css.chipPending : ""}`}
              data-testid="permission-chip"
              data-permission={liveMode}
              data-pending={permissionPending ? (permissionPending.queued ? "queued" : "switching") : undefined}
              aria-expanded={menu === "permission"}
              title={permOption?.description}
              onClick={() => toggle("permission")}
            >
              {mobile ? permTag ?? permLabel : `权限 ${permTag ?? permLabel}`} ▾
            </button>
          ) : (
            <span
              className={css.chip}
              data-testid="permission-chip"
              data-readonly="1"
              data-permission={liveMode}
              title={permOption?.description}
            >
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
      {menu === "usage" && usageRollup ? (
        <ContextUsagePopover
          rollup={usageRollup}
          mobile={mobile}
          onClose={dismissUsage}
          anchorUp={!mobile && placement === "up"}
          panelRef={menuRef}
          onMouseEnter={cancelHoverClose}
          onMouseLeave={scheduleHoverClose}
        />
      ) : null}
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
            modelEffective={caps.model ? modelEffective : null}
            modelPending={caps.model ? modelPending : null}
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
          {permOptions.map((m) => {
            const reachable = isLiveReachable(harness, m.id, launchPermissionMode);
            const active = liveMode === m.id;
            return (
              <button
                key={m.id}
                type="button"
                className={`${css.effortRow} ${active ? css.effortOn : ""}`}
                data-testid={`permission-option-${m.id}`}
                data-launch-only={m.launchOnly || !reachable ? "1" : undefined}
                disabled={!reachable}
                title={!reachable ? "该模式仅能在启动时选择" : m.description}
                onClick={() => {
                  if (!reachable) return;
                  onPermission?.(m.id);
                  setMenu(null);
                }}
              >
                <span className={`${css.radio} ${active ? css.radioOn : ""}`} />
                <span className={css.permissionName}>
                  {m.label}
                  {m.danger ? <span className={css.permissionDust}> · 危险</span> : null}
                  {m.launchOnly || !reachable ? (
                    <span className={css.launchOnlyTag}>仅启动时</span>
                  ) : null}
                </span>
                <span className={css.permissionNative}>{m.native}</span>
              </button>
            );
          })}
        </div>
      ) : null}
    </form>
  );
}
