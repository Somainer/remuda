import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
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
import { SpeechInput, readVoiceInputEnabled, speechRecognitionSupported } from "../../lib/speech";
import type { PromptMode } from "../../types/generated";
import type { CapabilitySnapshot } from "../../types/nativeRef";
import { AttachButtons, AttachmentChips, CodeQuoteChips } from "./AttachmentChips";
import { EffortSlider } from "./EffortSlider";
import { useAnchoredPopover } from "./AnchoredPopover";
import type { ModelCatalogView, ModelSelectionPath } from "./modelEffective";
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
import { composerState, steerHeldControl, type Phase } from "../composer/state";
import {
  effectiveLabel,
  effortMismatch,
  isEffortUnknown,
  type EffortEffectiveView,
} from "./effortEffective";
import { useAttachments } from "./useAttachments";
import { useCodeQuotes } from "./useCodeQuotes";
import { ContextUsagePopover } from "./ContextUsagePopover";
import { ComposerConfirmDialog, ComposerOptionsSheet } from "./ComposerOptions";
import type { UsageRollup } from "./contextUsage";
import type { AttachmentRef, Attachment } from "../../lib/attachments";
import css from "./session.module.css";
import opt from "./composerOptions.module.css";

type MenuId = "effort" | "permission" | "usage" | null;

/** A Sheet-based confirmation replacing a `window.confirm` (D-042). */
type ConfirmRequest = {
  kind: "steer" | "interrupt";
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm: () => void;
};

/** A held prompt row (c-steer). `id` is the local bubble id. */
export type HeldItem = {
  id: string;
  text: string;
  /** "turn" = 回合结束后送出; "answer" = pending question answered first. */
  reason: "turn" | "answer";
  /** remuda = client-held + cancelable; native = mirror of the harness queue. */
  holder: "remuda" | "native";
};

let mirrorSeq = 0;

export function Composer({
  instanceId,
  mobile,
  disabled,
  sending,
  onSend,
  onInterrupt,
  held = [],
  onHold,
  onRetractHeld,
  onSteerHeld,
  onFlushHeld,
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
  modelSelectionPath,
  modelCatalog,
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
  interrupted: interruptedProp,
  onInterruptedChange,
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
  ) => Promise<boolean | void> | boolean | void;
  /** D-028 §5.3 instance.cancel — interrupt the turn, process stays alive. */
  onInterrupt?: () => void | Promise<void>;
  /** Held queue rows, oldest first (c-steer). */
  held?: HeldItem[];
  /** Enter while busy/blocked: hold a message without POSTing. */
  onHold?: (
    text: string,
    reason: "turn" | "answer",
    refs: AttachmentRef[],
    staged: Attachment[],
  ) => void;
  /** Cancel one held row (Remuda-held only). */
  onRetractHeld?: (id: string) => void;
  /**
   * c-steer 插队发送: interrupt the running turn and send one held row now.
   * Resolves `false` when the steer POST did not land, so the 已打断 receipt
   * is only raised on a real interrupt.
   */
  onSteerHeld?: (id: string) => Promise<boolean | void> | boolean | void;
  /** Post every held row in order (the turn-end / answer transition). */
  onFlushHeld?: () => void | Promise<void>;
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
  /** Read-back marker: did the last Remuda switch pick a listed id or type it. */
  modelSelectionPath?: ModelSelectionPath | null;
  /** Resolved catalog with provenance, for the picker diagnostic note. */
  modelCatalog?: ModelCatalogView | null;
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
  /**
   * 已打断 receipt, optionally controlled by the parent so a 插队发送 started
   * from the transcript raises the same chip in the composer. When omitted the
   * composer owns it internally (standalone unit tests).
   */
  interrupted?: boolean;
  onInterruptedChange?: (interrupted: boolean) => void;
}) {
  const [text, setText] = useState(() => readDraft(instanceId));
  const [menu, setMenu] = useState<MenuId>(null);
  // D-042: phone options sheet (attachments / harness / permission / effort),
  // and the Sheet that replaces both window.confirm calls.
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [confirm, setConfirm] = useState<ConfirmRequest | null>(null);
  const optionsTriggerRef = useRef<HTMLButtonElement>(null);
  // Mirrors of prompts posted straight into a harness-native queue (codex
  // Tab): the wire owns them, so these chips are display-only and never
  // cancelable here. Remuda-held rows arrive through the `held` prop.
  const [mirrors, setMirrors] = useState<HeldItem[]>([]);
  // 已打断 receipt: controlled when the page owns it (so a 插队发送 clicked in
  // the transcript raises the same chip), otherwise local (standalone tests).
  const [internalInterrupted, setInternalInterrupted] = useState(false);
  const interrupted = interruptedProp ?? internalInterrupted;
  const setInterrupted = (value: boolean) => {
    if (onInterruptedChange) onInterruptedChange(value);
    else setInternalInterrupted(value);
  };
  // c-steer 插队发送: ids whose steer POST is in flight, so a double click on a
  // queued row's button sends exactly once (the row also leaves the queue as
  // soon as its POST lands, but the guard covers the pre-emit window).
  const steeringRef = useRef<Set<string>>(new Set());
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

  // ── §4.8 voice input (m-voice) ──────────────────────────────────────────
  // Platform keyboard dictation is the primary path and needs no code. The
  // mic is an enhancement rendered ONLY when the browser ships
  // SpeechRecognition AND the per-device pref is on, on phones. iOS Safari
  // has no SpeechRecognition (WebKit never implemented it) so nothing renders
  // there; desktop keeps an identical composer DOM. Both values are facts for
  // the component's life (the settings switch lives on another route), so
  // they are read once on mount.
  const voiceSupported = useMemo(() => speechRecognitionSupported(), []);
  const voicePrefOn = useMemo(() => readVoiceInputEnabled(), []);
  const voiceAvailable = mobile && voiceSupported && voicePrefOn;
  const [voiceListening, setVoiceListening] = useState(false);
  const speechInputRef = useRef<SpeechInput | null>(null);
  // Draft snapshot a running dictation session rewrites: the text captured
  // at start and the caret it started from. The base split is fixed for the
  // session — every update is prefix + transcript + original suffix.
  const dictationSpanRef = useRef<{ base: string; caret: number } | null>(null);

  /**
   * Write a recognition transcript into the draft through the SAME path as
   * typing (setText + writeDraft). Recognition never focuses the textarea —
   * stealing focus mid-dictation would raise the on-screen keyboard (§4.1) —
   * and never sends: the composing() key guard is simply not in play because
   * no key event is synthesised at all.
   */
  const applyDictation = (transcript: string) => {
    const span = dictationSpanRef.current;
    if (!span) return;
    // Interim updates replace the whole dictated span; the base's suffix
    // starts at the ORIGINAL caret and never moves with transcript length.
    const next =
      span.base.slice(0, span.caret) + transcript + span.base.slice(span.caret);
    textRef.current = next;
    setText(next);
    writeDraft(instanceId, next);
    // No focus() and no caret juggling: the textarea is not focused while the
    // mic owns the gesture (focusing would raise the on-screen keyboard,
    // §4.1), and a controlled value naturally rests the caret at its end.
  };

  const toggleDictation = () => {
    if (voiceListening) {
      speechInputRef.current?.stop();
      return;
    }
    const area = inputRef.current;
    const caret =
      area && area.selectionStart != null ? area.selectionStart : textRef.current.length;
    dictationSpanRef.current = { base: textRef.current, caret };
    const speech = new SpeechInput({
      onTranscript: applyDictation,
      onError: () => {
        dictationSpanRef.current = null;
        setVoiceListening(false);
      },
      onEnd: () => {
        dictationSpanRef.current = null;
        setVoiceListening(false);
      },
    });
    speechInputRef.current = speech;
    try {
      // Constructed on a click so start() is inside the user-gesture window
      // the mic-permission prompt requires.
      speech.start();
      setVoiceListening(true);
    } catch {
      // A rejected start (permission race) leaves no session and a resting
      // button; the partial transcript span is discarded.
      dictationSpanRef.current = null;
      speechInputRef.current = null;
      setVoiceListening(false);
    }
  };

  useEffect(
    () => () => {
      // abort, not stop: leaving the page must not wait for a final flush.
      speechInputRef.current?.abort();
    },
    [],
  );
  const menuRefs = {
    effort: useRef<HTMLDivElement>(null),
    permission: useRef<HTMLDivElement>(null),
    usage: useRef<HTMLDivElement>(null),
  };
  const triggerRefs = {
    effort: useRef<HTMLButtonElement>(null),
    permission: useRef<HTMLButtonElement>(null),
    usage: useRef<HTMLButtonElement>(null),
  };
  // A pending approval/question card parks above the composer; a menu opening
  // upward must not overlap it. The getter re-queries the DOM at measure time
  // (the card lives in a different React subtree and can appear late).
  const approvalRef = useMemo<{ readonly current: HTMLElement | null }>(
    () => ({
      get current() {
        return document.querySelector<HTMLElement>(
          "[data-testid='approval-card'], [data-testid='question-form']",
        );
      },
    }),
    [],
  );
  // Each menu is anchored to ITS OWN trigger chip, flips on the room the
  // measured panel actually needs (re-measured on the slider/list flip via
  // ResizeObserver), and is height-capped to the available viewport room.
  const effortAnchor = useAnchoredPopover(
    triggerRefs.effort,
    menuRefs.effort,
    menu === "effort",
    { align: "start", preferUp: true, avoidElements: [approvalRef] },
  );
  const permissionAnchor = useAnchoredPopover(
    triggerRefs.permission,
    menuRefs.permission,
    menu === "permission",
    { align: "end", preferUp: true, avoidElements: [approvalRef] },
  );
  const usageAnchor = useAnchoredPopover(
    triggerRefs.usage,
    menuRefs.usage,
    menu === "usage",
    mobile ? { sheet: true } : { align: "end", preferUp: false },
  );
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
  // D-042: yolo-class modes (绕过全部 / 不再询问 / 完全访问) must read as
  // danger on the collapsed phone trigger, not only inside the sheet.
  const permDanger = permOption?.danger === true;
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
  // c-steer 插队发送 for an already-queued row: same availability rule as the
  // box's 插队, resolved once here so every queued row shows one honest reason.
  const heldSteer = steerHeldControl(
    harness,
    phase,
    capabilities ?? ({ capabilities: {} } as unknown as CapabilitySnapshot),
  );
  // Latest-guard refs so a Sheet confirm re-checks the LIVE turn state at the
  // moment the user confirms (unlike a blocking window.confirm, a Sheet lets
  // the turn end while the dialog is open).
  const controlsRef = useRef(controls);
  controlsRef.current = controls;
  const canSubmitRef = useRef<() => boolean>(() => false);

  const busy = phase === "working" || phase === "blocked";
  const remudaHeld = held.filter((item) => item.holder === "remuda");
  const heldRows: HeldItem[] = [...held, ...mirrors];
  /** 1-based queue ordinal among turn-wait Remuda-held rows. */
  const ordinalOf = (id: string) =>
    remudaHeld.filter((item) => item.reason === "turn").findIndex((item) => item.id === id) + 1;

  // A Sheet confirm is only valid while its precondition holds. If the turn
  // state changes while the dialog is open, dismiss it rather than let a later
  // click post a steer/cancel against the wrong state (D-042): 插队 needs a
  // working turn, 打断 needs any busy turn.
  useEffect(() => {
    if (!confirm) return;
    if (confirm.kind === "steer" && phase !== "working") setConfirm(null);
    if (confirm.kind === "interrupt" && !busy) setConfirm(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phase]);

  // c-steer: held prompts flush when the wait ends — a working turn goes idle
  // (or is interrupted into a steer), a pending question resolves (blocked →
  // working/idle). Delivery keeps queue order; see store.flushHeld.
  useEffect(() => {
    const prev = phaseRef.current;
    phaseRef.current = phase;
    const turnEnded = prev === "working" && phase === "idle";
    const answered = prev === "blocked" && (phase === "idle" || phase === "working");
    if (!turnEnded && !answered) return;
    if (remudaHeld.length === 0) {
      setMirrors([]);
      return;
    }
    setMirrors([]);
    void onFlushHeld?.();
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
  canSubmitRef.current = canSubmit;

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

  /**
   * The interrupt Sheet confirm must act on the LIVE turn state: if the turn
   * ended while the dialog was open there is nothing to cancel. Read the
   * phase through its ref (not the render closure) so the guard is current at
   * click time.
   */
  const confirmInterrupt = () => {
    const livePhase = phaseRef.current;
    if ((livePhase !== "working" && livePhase !== "blocked") || !controlsRef.current.interrupt.available) {
      return;
    }
    void doInterrupt();
  };

  /**
   * Submit the PRIMARY control.
   * Idle → send a normal new turn immediately.
   * Working/blocked → queue: Remuda holds it (reason = turn end / question
   * answered); a harness-native queue (codex Tab) is posted with mode:queue
   * and the chip becomes a display-only ledger mirror.
   */
  const submitPrimary = async () => {
    if (!canSubmit()) return;
    const value = expandCodeQuotes(text.trim(), codeQuotes.quotes);
    const refs = images.refs(value);
    const staged = images.attachments;
    const action = controls.primary;
    if (action.kind === "queue") {
      if (action.holder === "native") {
        setMirrors((cur) => [
          ...cur,
          { id: `mirror_${++mirrorSeq}`, text: value, reason: "turn", holder: "native" },
        ]);
        clearBox();
        void onSend(value, refs, staged, "queue");
        return;
      }
      onHold?.(value, phase === "blocked" ? "answer" : "turn", refs, staged);
      clearBox();
      return;
    }
    clearBox();
    await onSend(value, refs, staged, action.mode);
  };

  /**
   * c-steer 插队 (Cmd/Ctrl+Enter or the visible button): open the Sheet
   * confirmation, then interrupt the running turn through the driver's own
   * key path and deliver this message first — the Node sends Esc, waits for
   * turn-ended, and jumps it ahead of the held queue. Blocked (a question is
   * open) never offers it. The draft is only cleared on confirm, so cancel
   * leaves the text and staged attachments untouched.
   */
  const submitSteer = async () => {
    if (!canSubmit() || !controls.steer.available || phase !== "working") return;
    setConfirm({
      kind: "steer",
      title: "插队发送",
      message: "打断当前 turn 并立即发送（插队）？已排队的消息仍会按顺序随后送出。",
      confirmLabel: "打断并发送",
      onConfirm: () => {
        // Re-check the LIVE state: a Sheet (unlike the old blocking
        // window.confirm) lets the turn end while the user reads the dialog.
        if (phaseRef.current !== "working" || !controlsRef.current.steer.available) return;
        if (!canSubmitRef.current()) return;
        const value = expandCodeQuotes(textRef.current.trim(), codeQuotes.quotes);
        const refs = images.refs(value);
        const staged = images.attachments;
        clearBox();
        // 已打断 is a receipt for an interrupt that actually happened: only
        // raise it once the steer POST landed (a failure leaves 状态待确认).
        void (async () => {
          const landed = await onSend(value, refs, staged, "steer");
          if (landed !== false) setInterrupted(true);
        })();
      },
    });
  };

  const removeHeld = (id: string) => {
    onRetractHeld?.(id);
  };

  /**
   * c-steer 插队发送 on a queued row: one click is the whole gesture (the row
   * already shows its text, so no confirm). The in-flight guard makes a double
   * click send once; the row leaves the queue the moment its POST lands. The
   * 已打断 chip is a receipt raised only after the steer POST actually lands
   * (a failed POST leaves the row 状态待确认 under no such claim). The composer
   * text box and the other rows are untouched.
   */
  const steerHeldRow = async (id: string) => {
    if (!heldSteer.enabled || !onSteerHeld) return;
    if (steeringRef.current.has(id)) return;
    steeringRef.current.add(id);
    const landed = await onSteerHeld(id);
    if (landed !== false) setInterrupted(true);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing(event)) return;
    // Esc while the composer is focused = 打断, with a Sheet confirm
    // (desktop popover / phone sheet). Handled BEFORE the mobile early
    // return: unlike Enter (a newline on phones), Esc is never a character,
    // and a phone with a hardware keyboard sends a real Esc. window.confirm
    // is gone (D-042): the native dialog covered the keyboard on Safari.
    if (event.key === "Escape" && busy && controls.interrupt.available) {
      event.preventDefault();
      event.stopPropagation();
      setConfirm({
        kind: "interrupt",
        title: "打断当前 turn",
        message: "打断当前 turn？会话与进程不会退出。",
        confirmLabel: "打断",
        onConfirm: confirmInterrupt,
      });
      return;
    }
    // The remaining shortcuts are desktop-keyboard semantics; on a phone
    // Enter types a newline and Cmd/Ctrl+Enter is not bound.
    if (mobile) return;
    // Cmd/Ctrl+Enter while working = 插队: interrupt and send this first.
    if (
      event.key === "Enter"
      && (event.metaKey || event.ctrlKey)
      && !event.shiftKey
      && phase === "working"
      && controls.steer.available
    ) {
      event.preventDefault();
      void submitSteer();
      return;
    }
    // Enter = primary (idle: send; busy/blocked: queue). Shift+Enter newline.
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
      if (event.key !== "Escape") return;
      // Return focus to the chip that opened the menu before it unmounts.
      if (menu) {
        const chipFor: Record<NonNullable<MenuId>, React.RefObject<HTMLElement | null>> = {
          effort: triggerRefs.effort,
          permission: triggerRefs.permission,
          usage: triggerRefs.usage,
        };
        chipFor[menu].current?.focus();
      }
      dismissUsage();
    };
    window.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  const toggle = (id: MenuId) => {
    if (id !== "usage") usagePinned.current = false;
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
  const primaryLabel = sending ? "发送中" : controls.primary.label;
  const primaryTestId =
    controls.primary.kind === "queue" ? "composer-queue" : "composer-send";

  const modelLockedReason = caps.model
    ? effortDisabled
      ? "会话已退出或为只读会话（observed-only），无法下发 instance.configure"
      : null
    : "此会话不支持模型切换（无 instance.configure 能力）";

  // ── D-042: slot nodes shared by the desktop bar and the phone sheet ──────
  // The permission rows and the effort slider are the SAME controls in both
  // surfaces: desktop keeps its anchored popovers, the phone renders them
  // once inside the options Sheet.
  const attachHandlers = {
    onFiles: (files: File[]) => insertForAttachments(images.add(files)),
    onPasteClick: async () => insertForAttachments(await images.pasteFromClipboard()),
  };
  // Attachments started from the options Sheet must dismiss the Sheet FIRST:
  // otherwise insertForAttachments focuses/scrolls the textarea behind the
  // aria-modal scrim, violating the focus trap. After close the textarea
  // focus is correct (the staged chip renders above the input).
  const closeOptions = () => setOptionsOpen(false);
  const sheetAttachHandlers = {
    onFiles: (files: File[]) => {
      closeOptions();
      insertForAttachments(images.add(files));
    },
    onPasteClick: async () => {
      closeOptions();
      insertForAttachments(await images.pasteFromClipboard());
    },
  };

  const harnessChipNode = caps.harness ? (
    <span className={css.chip} data-testid="harness-chip" data-readonly="1">
      <span className={css.chipMark}>{harnessChip.mark}</span>
      <span>{mobile ? harnessChip.label.replace(" Code", "") : harnessChip.label}</span>
    </span>
  ) : null;

  const effortSliderNode = caps.effort ? (
    <EffortSlider
      kind={harness}
      model={caps.model ? model : undefined}
      models={caps.model ? models : undefined}
      modelEffective={caps.model ? modelEffective : null}
      modelPending={caps.model ? modelPending : null}
      modelSelectionPath={caps.model ? modelSelectionPath : null}
      modelCatalog={caps.model ? (modelCatalog ?? null) : null}
      modelLockedReason={modelLockedReason}
      index={currentEffort.index}
      ultracode={ultraOn}
      disabled={effortLocked}
      onChange={(next) => onEffort?.(next)}
      onModel={caps.model ? onModel : undefined}
      onClose={() => {
        setMenu(null);
        triggerRefs.effort.current?.focus();
      }}
    />
  ) : null;

  // The same slider inside the phone options Sheet closes the SHEET and
  // returns focus to its collapsed trigger (Sheet focus trap also restores
  // focus, but the slider's own Escape path names the trigger explicitly).
  const sheetEffortSliderNode = caps.effort ? (
    <EffortSlider
      kind={harness}
      model={caps.model ? model : undefined}
      models={caps.model ? models : undefined}
      modelEffective={caps.model ? modelEffective : null}
      modelPending={caps.model ? modelPending : null}
      modelSelectionPath={caps.model ? modelSelectionPath : null}
      modelCatalog={caps.model ? (modelCatalog ?? null) : null}
      modelLockedReason={modelLockedReason}
      index={currentEffort.index}
      ultracode={ultraOn}
      disabled={effortLocked}
      onChange={(next) => onEffort?.(next)}
      onModel={caps.model ? onModel : undefined}
      onClose={() => {
        setOptionsOpen(false);
        optionsTriggerRef.current?.focus();
      }}
    />
  ) : null;

  const renderPermRows = (inSheet: boolean) =>
    permOptions.map((m) => {
      const reachable = isLiveReachable(harness, m.id, launchPermissionMode);
      const active = liveMode === m.id;
      return (
        <button
          key={m.id}
          type="button"
          className={`${css.effortRow} ${active ? css.effortOn : ""} ${inSheet ? opt.sheetPermRow : ""}`}
          data-testid={`permission-option-${m.id}`}
          data-launch-only={m.launchOnly || !reachable ? "1" : undefined}
          disabled={!reachable}
          title={!reachable ? "该模式仅能在启动时选择" : m.description}
          onClick={() => {
            if (!reachable) return;
            onPermission?.(m.id);
            setMenu(null);
            if (inSheet) setOptionsOpen(false);
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
    });

  const permReadonlyNode = (
    <span
      className={css.chip}
      data-testid="permission-chip"
      data-readonly="1"
      data-permission={liveMode}
      title={permOption?.description}
    >
      {permLabel}
    </span>
  );

  const effortSparks = ember ? (
    <>
      <span className={css.emberSpark} />
      <span className={`${css.emberSpark} ${css.emberSpark2}`} />
      <span className={`${css.emberSpark} ${css.emberSpark3}`} />
    </>
  ) : null;

  const effortWordNode = (
    <>
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
    </>
  );

  // Context usage rides INSIDE the phone options Sheet (dispatch plan §C
  // default 4), not on the collapsed trigger. Tapping it opens the usage
  // detail popover as a stacked bottom sheet on touch widths.
  const contextChipInner = (
    <>
      <span
        className={css.contextRing}
        style={{ ["--ctx-pct" as string]: contextPct == null ? "0%" : `${contextPct}%` }}
      />
      <span>{contextChipLabel}</span>
    </>
  );
  const contextChipNode = caps.context ? (
    <button
      type="button"
      className={`${css.chip} ${opt.sheetTouch}`}
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
        if (usageRollup) {
          usagePinned.current = true;
          cancelHoverClose();
          setMenu("usage");
        }
      }}
    >
      {contextChipInner}
    </button>
  ) : null;
  // Desktop keeps its hover-open chip anchored to its own trigger.
  const desktopContextChip = caps.context ? (
    <button
      ref={triggerRefs.usage}
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
        // Idempotent, click-pinned open: mouseenter may already have opened
        // it on precise pointers, and touch fires no hover. Pinning means a
        // later pointer leave cannot dismiss the card; × / outside
        // pointerdown / Escape unpin and close.
        if (usageRollup) {
          usagePinned.current = true;
          cancelHoverClose();
          setMenu("usage");
        }
      }}
      onMouseEnter={() => {
        if (usageRollup && hoverCapable()) {
          cancelHoverClose();
          setMenu("usage");
        }
      }}
      onMouseLeave={scheduleHoverClose}
    >
      {contextChipInner}
    </button>
  ) : null;

  // D-042 phone trigger: one fused chip that ALWAYS names the current
  // permissionMode word AND the effort tier (`manual · high`). Danger modes
  // take danger styling right here, collapsed — the bypass state must never
  // be visible only after opening the sheet.
  const triggerModeWord = caps.permission ? permTag ?? permLabel : null;
  const mobileTriggerNode = (
    <span className={opt.triggerGroup}>
      <button
        ref={optionsTriggerRef}
        type="button"
        className={`${opt.trigger} ${permDanger ? opt.triggerDanger : ""} ${ember ? opt.triggerEmber : ""} ${pendingLabel ? css.chipEffortPending : ""}`}
        data-testid={caps.effort ? "model-effort-chip" : "composer-options-trigger"}
        data-options-trigger="1"
        data-ember={ember ? "1" : "0"}
        data-permission={caps.permission ? liveMode : undefined}
        data-permission-danger={caps.permission && permDanger ? "1" : "0"}
        data-effort-effective={
          caps.effort ? (pendingLabel ? "pending" : effectiveUnknown ? "unknown" : effectiveWord) : undefined
        }
        data-effort-pending={caps.effort && pendingLabel ? (effortPending?.queued ? "queued" : "switching") : "0"}
        data-effort-source={caps.effort ? effortEffective?.source ?? "unknown" : undefined}
        data-effort-mismatch={caps.effort ? (mismatch ? "1" : "0") : undefined}
        aria-haspopup="dialog"
        aria-expanded={optionsOpen}
        // D-042: the accessible name must carry the permission word (and a
        // explicit 危险 marker for yolo modes) as well as the effort tier,
        // since this one trigger replaces both desktop chips.
        aria-label={`${triggerModeWord ? `${triggerModeWord}${permDanger ? "（危险）" : ""} · ` : ""}Select effort, ${effortChipLabel}; effective ${pendingLabel ? `pending ${pendingLabel}` : effectiveUnknown ? "unknown" : effectiveWord}`}
        title={[permOption?.description, effortChipTitle].filter(Boolean).join("\n")}
        // The trigger stays clickable for an exited/observed-only session so
        // the sheet can still show the locked effort slider / launch-only
        // permission rows; the actions inside carry their own disabled state.
        onClick={() => setOptionsOpen(true)}
      >
        {effortSparks}
        {triggerModeWord ? (
          <span className={opt.triggerMode} data-testid="composer-trigger-permission">
            {triggerModeWord}
          </span>
        ) : null}
        {triggerModeWord && caps.effort ? <span className={opt.triggerSep}>·</span> : null}
        {caps.effort ? effortWordNode : null}
        {!caps.permission && !caps.effort ? (
          <span className={opt.triggerMode}>选项</span>
        ) : null}
        <span className={css.chipCaret}>▾</span>
      </button>
    </span>
  );

  // D-028a: the three-state controls, queue chip and 「尚未验证」 note are
  // facts of the current turn, not "options" — on the phone they stay OUTSIDE
  // the sheet, identical on both surfaces.
  // §4.8 (m-voice): the mic sits beside the textarea, only where
  // speechRecognitionSupported() and the settings opt-in agree. Its copy
  // states the one guarantee that matters — text only, never auto-sent.
  const voiceNode = voiceAvailable ? (
    <button
      type="button"
      className={css.chip}
      data-testid="composer-voice"
      data-listening={voiceListening ? "1" : "0"}
      aria-pressed={voiceListening}
      disabled={disabled}
      title="语音输入：识别文字只填入输入框，不会自动发送"
      aria-label="语音输入：识别文字只填入输入框，不会自动发送"
      onClick={toggleDictation}
    >
      <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
        <path
          d="M8 2.5a2 2 0 0 0-2 2v3.5a2 2 0 0 0 4 0V4.5a2 2 0 0 0-2-2Z"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.3"
        />
        <path
          d="M4.5 7.5a3.5 3.5 0 0 0 7 0M8 11v2.5M6.5 13.5h3"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.3"
          strokeLinecap="round"
        />
      </svg>
    </button>
  ) : null;
  const voiceHintNode =
    voiceAvailable && voiceListening ? (
      <span className={css.controlNote} data-testid="composer-voice-hint">
        正在聆听… 文字只进输入框，不会自动发送；再点麦克风结束
      </span>
    ) : null;

  const queueStatusNode = heldRows.length ? (
    <span className={css.chip} data-testid="composer-queue-status">
      已排队 {heldRows.length}
    </span>
  ) : null;
  const interruptedNode = interrupted ? (
    <span className={css.chip} data-testid="composer-interrupted-chip">
      已打断
    </span>
  ) : null;
  const capNoteNode = controls.note ? (
    <span className={css.controlNote} data-testid="composer-cap-note" title={controls.note}>
      {controls.note}
    </span>
  ) : null;
  const actionButtonsNode = (
    <>
      {/* c-steer controls: Enter queues; 插队 interrupts and jumps; 打断 ends. */}
      {busy && controls.steer.available ? (
        <button
          type="button"
          className={css.queueBtn}
          data-testid="composer-steer"
          data-provision={controls.steer.provision}
          disabled={disabled || sending || images.uploading || !text.trim()}
          title={
            controls.steer.note
              ? `插队：打断当前 turn 并立即发送（${controls.steer.note}）`
              : "插队：打断当前 turn 并立即发送（⌘/Ctrl+Enter）"
          }
          onClick={() => void submitSteer()}
        >
          插队
          {mobile ? null : <span className={css.controlSub}>⌘/Ctrl+↵</span>}
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
        data-mode={controls.primary.kind === "queue" ? "queue" : controls.primary.mode}
        data-holder={controls.primary.kind === "queue" ? controls.primary.holder : undefined}
        disabled={disabled || sending || images.uploading || (!text.trim() && images.attachments.length === 0)}
      >
        {primaryLabel}
      </button>
    </>
  );
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
      {heldRows.length ? (
        <div className={css.queuedRow} data-testid="composer-queued-row">
          {heldRows.map((item) => {
            const nth = item.holder === "remuda" && item.reason === "turn" ? ordinalOf(item.id) : null;
            const tag =
              item.holder === "native"
                ? "原生"
                : item.reason === "answer"
                  ? "待回答后送出"
                  : nth
                    ? `排队中 · 第 ${nth} 条 · 回车后送出`
                    : "排队中";
            return (
              <span
                key={item.id}
                className={css.queuedChip}
                data-testid="composer-queued-chip"
                data-holder={item.holder}
                data-reason={item.reason}
                data-ordinal={nth ?? undefined}
                title={item.holder === "remuda" ? "Remuda 代持：可撤回" : "harness 原生队列"}
              >
                <span className={css.queuedTag}>{tag}</span>
                <span className={css.queuedText}>{item.text}</span>
                {item.holder === "remuda" ? (
                  <>
                    <button
                      type="button"
                      className={css.queuedSteer}
                      data-testid="composer-queued-steer"
                      disabled={!heldSteer.enabled}
                      aria-label={
                        heldSteer.enabled
                          ? "插队发送这条排队消息"
                          : `插队发送这条排队消息（不可用：${heldSteer.reason}）`
                      }
                      title={
                        heldSteer.enabled
                          ? heldSteer.reason
                            ? `插队发送，打断当前 turn 并立即发送（${heldSteer.reason}）`
                            : "插队发送，打断当前 turn 并立即发送"
                          : heldSteer.reason
                      }
                      onClick={() => steerHeldRow(item.id)}
                    >
                      插队发送
                    </button>
                    <button
                      type="button"
                      className={css.queuedRemove}
                      data-testid="composer-queued-remove"
                      aria-label="撤回排队消息"
                      onClick={() => removeHeld(item.id)}
                    >
                      ✕
                    </button>
                  </>
                ) : null}
              </span>
            );
          })}
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
          // D-042: phones get a plain hint — Enter is newline there and ⌘
          // does not exist, so advertising desktop shortcuts would mislead.
          placeholder={
            mobile
              ? "输入提示词…"
              : "输入提示词…  Enter 排队 · ⌘/Ctrl+Enter 插队 · 工作中 Esc 打断 · IME 组字期间不送"
          }
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
        {voiceNode}
      </div>
      <div className={css.controlBar} data-testid="composer-bar" data-collapsed={mobile ? "1" : "0"}>
        {mobile ? (
          <>
            {/* D-042: one trigger + input + primary; options live in sheet. */}
            {mobileTriggerNode}
            {voiceHintNode}
            {queueStatusNode}
            {interruptedNode}
            {capNoteNode}
            <span className={css.barSpacer} />
            {actionButtonsNode}
          </>
        ) : (
          <>
            <AttachButtons
              className={css.chip}
              disabled={disabled}
              mobile={mobile}
              onFiles={attachHandlers.onFiles}
              onPasteClick={attachHandlers.onPasteClick}
            />
            {harnessChipNode}
            {caps.effort ? (
              <button
                ref={triggerRefs.effort}
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
                {effortSparks}
                {effortWordNode}
                <span className={css.chipCaret}>▾</span>
              </button>
            ) : null}
            {desktopContextChip}
            {caps.permission ? (
              onPermission ? (
                <button
                  ref={triggerRefs.permission}
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
                permReadonlyNode
              )
            ) : null}
            {queueStatusNode}
            {interruptedNode}
            {capNoteNode}
            <span className={css.barSpacer} />
            {actionButtonsNode}
          </>
        )}
      </div>
      {menu === "usage" && usageRollup ? (
        <ContextUsagePopover
          rollup={usageRollup}
          mobile={mobile}
          onClose={dismissUsage}
          placement={usageAnchor.placement}
          anchorStyle={usageAnchor.style}
          panelRef={menuRefs.usage}
          onMouseEnter={cancelHoverClose}
          onMouseLeave={scheduleHoverClose}
        />
      ) : null}
      {!mobile && menu === "effort" ? (
        <div
          ref={menuRefs.effort}
          style={effortAnchor.style}
          className={`${css.popover} ${css.popoverCard}`}
          data-testid="effort-menu"
          data-placement={effortAnchor.placement}
        >
          {effortSliderNode}
        </div>
      ) : null}
      {!mobile && menu === "permission" ? (
        <div
          ref={menuRefs.permission}
          style={permissionAnchor.style}
          className={css.popover}
          data-testid="permission-menu"
          data-placement={permissionAnchor.placement}
        >
          <div data-popover-scroll="1">{renderPermRows(false)}</div>
        </div>
      ) : null}
      <ComposerOptionsSheet
        open={mobile && optionsOpen}
        onClose={() => setOptionsOpen(false)}
        returnFocusRef={optionsTriggerRef}
        attach={
          <AttachButtons
            className={opt.attachBtn}
            disabled={disabled}
            mobile
            onFiles={sheetAttachHandlers.onFiles}
            onPasteClick={sheetAttachHandlers.onPasteClick}
          />
        }
        harness={harnessChipNode}
        context={contextChipNode}
        permission={
          onPermission ? (
            // composerOptions-owned frame: no desktop popover border/radius or
            // 300px cap inside the sheet (that made a double frame + dead strip).
            <div className={opt.sheetMenu} data-testid="permission-menu" data-in-sheet="1">
              <div data-popover-scroll="1">{renderPermRows(true)}</div>
            </div>
          ) : caps.permission ? (
            permReadonlyNode
          ) : null
        }
        effort={
          caps.effort ? (
            // Full-width, frameless in the sheet; the slider's own card
            // supplies its visuals.
            <div className={opt.sheetCard} data-testid="effort-menu" data-placement="up" data-in-sheet="1">
              {sheetEffortSliderNode}
            </div>
          ) : null
        }
      />
      <ComposerConfirmDialog
        open={confirm !== null}
        variant={mobile ? "sheet" : "popover"}
        title={confirm?.title ?? ""}
        message={confirm?.message ?? ""}
        confirmLabel={confirm?.confirmLabel ?? ""}
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          const action = confirm?.onConfirm;
          setConfirm(null);
          action?.();
        }}
      />
    </form>
  );
}
