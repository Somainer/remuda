import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  clampEffortIndex,
  defaultEffortIndex,
  effortIndexFromClientX,
  effortLook,
  effortRatio,
  effortStops,
  keyboardEffortIndex,
  modelsFor,
  shortModel,
  ULTRACODE_COUPLED_DESC,
  ULTRACODE_SWITCH_DESC,
  ULTRACODE_SWITCH_LABEL,
  type ClaudeVersionGate,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import type { ModelCatalogView, ModelSelectionPath } from "./modelEffective";
import css from "./effort.module.css";

/**
 * The effort slider with the D-056 orthogonal Ultracode switch.
 *
 * Claude: FIVE native stops (low…max) + a separate `role="switch"` row under
 * the pill. On ≥2.1.284 the switch is orthogonal (on at any level); on the
 * coupled 2.1.203–2.1.283 builds the parent links it to xhigh; below 2.1.203
 * the switch is disabled with the named reason. Codex keeps its six native
 * tiers and renders no switch.
 */
export function EffortSlider({
  kind,
  index,
  ultracode = false,
  onUltracodeChange,
  ultraGate = "unknown",
  ultraBlocked = null,
  ultraEffective = null,
  defaultIndex,
  disabled,
  idPrefix = "effort",
  label = "effort",
  footer,
  variant = "popover",
  model,
  launchModel = null,
  models,
  modelEffective,
  modelPending,
  modelSelectionPath,
  modelCatalog,
  modelLockedReason = null,
  onChange,
  onModel,
  onClose,
}: {
  kind: EffortKind | string;
  /** Slider tier index. */
  index: number;
  /** Optimistic/current switch state (Claude only). */
  ultracode?: boolean;
  /** Flip the switch; absent means there is no configure channel. */
  onUltracodeChange?: (on: boolean) => void;
  /** Claude Code version classification driving the switch rules. */
  ultraGate?: ClaudeVersionGate;
  /** Model/process-scoped refusal that keeps the switch off. */
  ultraBlocked?: { reason: string; model?: string | null } | null;
  /** Positively observed switch state (null = no process-local evidence). */
  ultraEffective?: boolean | null;
  /** Marked default tier index; undefined = table fallback, null = no marker. */
  defaultIndex?: number | null;
  disabled?: boolean;
  idPrefix?: string;
  label?: string;
  footer?: string;
  variant?: "popover" | "inline";
  model?: string;
  launchModel?: string | null;
  models?: string[];
  modelEffective?: string | null;
  modelPending?: { id: string; queued: boolean } | null;
  modelSelectionPath?: ModelSelectionPath | null;
  modelCatalog?: ModelCatalogView | null;
  modelLockedReason?: string | null;
  onChange: (next: EffortSelection) => void;
  onModel?: (model: string) => void;
  onClose?: () => void;
}) {
  const tid = (suffix: string) => `${idPrefix}-${suffix}`;
  const inline = variant === "inline";
  const frame = inline ? css.effortForm : css.effortCard;
  const stops = useMemo(() => effortStops(kind), [kind]);
  const [draft, setDraft] = useState<number | null>(null);
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef(false);
  const [list, setList] = useState(false);
  const [typeValue, setTypeValue] = useState("");

  const KNOB_INSET = 18;
  const isClaude = kind === "claude";
  const switchVisible = isClaude;

  const locked = Boolean(disabled) || stops.length === 0;
  const shown = clampEffortIndex(draft ?? index, Math.max(1, stops.length));
  const stop = stops[shown];
  const stopLabel = stop?.label ?? stop?.name ?? "effort";
  const look = effortLook(kind, shown, false);
  const top = look === "top";
  // Ember fills the pill only for a native ember TIER (Codex Ultra). Claude
  // ember lives on the SWITCH and the collapsed trigger, never on the pill.
  const pillEmber = look === "ultracode";

  const fallbackIndex =
    defaultIndex === null
      ? null
      : clampEffortIndex(defaultIndex ?? defaultEffortIndex(kind), Math.max(1, stops.length));
  const fallback = fallbackIndex ?? clampEffortIndex(defaultEffortIndex(kind), stops.length);

  // ── Switch availability (D-056 §5) ────────────────────────────────────
  const switchLockedReason: string | null = (() => {
    if (!switchVisible) return null;
    if (locked) return "会话已退出或为只读会话（observed-only），无法切换 ultracode";
    if (ultraGate === "legacy") return "Claude Code 2.1.203 之前不支持 ultracode";
    // D-056 r2 item 1: an unreported/unparsable version must never be treated
    // as decoupled-capable. Disable ONLY this switch, name why — the five
    // effort stops stay usable.
    if (ultraGate === "unknown") return "未获取到 Claude Code 版本，无法确认是否支持 ultracode";
    if (ultraBlocked?.reason === "ultracode-unavailable-for-model") {
      return `模型 ${ultraBlocked.model ?? "当前模型"} 不支持 ultracode`;
    }
    if (ultraBlocked?.reason === "ultracode-workflows-disabled") {
      return "需要开启 dynamic workflows（该进程的 workflows 已被关闭，重新启动进程后恢复）";
    }
    return null;
  })();
  const switchDisabled = !onUltracodeChange || switchLockedReason !== null;
  const switchEffective: "on" | "off" | "unknown" =
    ultraEffective === true ? "on" : ultraEffective === false ? "off" : "unknown";

  const snapFromClientX = (clientX: number): number => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect) return shown;
    return effortIndexFromClientX(
      clientX,
      { left: rect.left + KNOB_INSET, width: rect.width - KNOB_INSET * 2 },
      stops.length,
    );
  };

  // A tier drag changes ONLY the tier axis; the current flag rides along so
  // the parent can apply the coupled-build linkage (slide away → off) while a
  // decoupled flip never moves the slider. Compare against the PROP index (a
  // drag sets a draft before pointer-up, so comparing against the shown value
  // would suppress the actual emit).
  const emitTier = (next: number) => {
    const clamped = clampEffortIndex(next, stops.length);
    if (clamped === index) return;
    onChange({
      index: clamped,
      name: stops[clamped]?.name ?? "effort",
      kind: (kind as EffortKind) || "claude",
      ...(isClaude ? { ultracode } : {}),
    });
  };

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (locked) return;
    event.preventDefault();
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      /* jsdom */
    }
    dragRef.current = true;
    const next = snapFromClientX(event.clientX);
    setDraft(next);
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current || locked) return;
    setDraft(snapFromClientX(event.clientX));
  };
  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return;
    dragRef.current = false;
    const next = snapFromClientX(event.clientX);
    setDraft(next);
    emitTier(next);
  };
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (locked) return;
    const next = keyboardEffortIndex(shown, event.key, stops.length);
    if (next == null) return;
    event.preventDefault();
    setDraft(next);
    emitTier(next);
  };

  const hasModelAxis = model !== undefined;
  const currentModel = hasModelAxis ? shortModel(model) : "";

  // A local drag/keyboard draft survives within one gesture but resets once
  // a controlled value lands (tier or flag prop change). A mock parent that
  // never updates keeps the gesture advancing; an external change takes over.
  useEffect(() => {
    setDraft(null);
  }, [index, ultracode]);

  // When the tier/model list opens, focus the current tier for roving
  // keyboard navigation.
  useEffect(() => {
    if (!list) return;
    // Focus the current tier row when the list opens (roving keyboard).
    const node = Array.from(
      listBodyRef.current?.querySelectorAll<HTMLButtonElement>("button[data-testid^='effort-tier-']") ?? [],
    )[shown];
    node?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [list]);
  const runningModel = modelEffective || launchModel || "";
  const chipText = hasModelAxis ? runningModel : (stop?.description ?? "");
  const chipTitle = hasModelAxis
    ? runningModel
      ? modelEffective
        ? `实际 ${runningModel}`
        : `${runningModel}（尚未从会话回读）`
      : ""
    : stop?.description;
  const catalogDiagnostic = (() => {
    if (!modelCatalog) return null;
    if (modelCatalog.source === "gateway-discovery" && modelCatalog.discoveryEnv === false) {
      return { reason: "discovery-env-missing", text: "本会话环境缺少 CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY，列表可能不是网关实时发现的" };
    }
    if (modelCatalog.cache?.scope === "host-fallback") {
      const base = modelCatalog.cache.baseUrl ? `（relay ${modelCatalog.cache.baseUrl}）` : "";
      return { reason: "host-fallback", text: `列表来自主机缓存而非本会话发现${base}，直接输入会交由终端裁决` };
    }
    return null;
  })();

  const modelList = useMemo(() => {
    if (!currentModel) return [];
    // An explicit catalog list is used as-is (deduped); with no list, offer
    // the harness's built-in aliases plus the running id.
    const base = models && models.length ? models : modelsFor(kind, [currentModel]);
    const out: string[] = [];
    for (const id of base) if (id && !out.includes(id)) out.push(id);
    return out;
  }, [currentModel, kind, models]);
  const modelPendingShort = modelPending ? shortModel(modelPending.id) : null;

  // ── Slider track (pill) ───────────────────────────────────────────────
  const ratio = effortRatio(shown, Math.max(1, stops.length));
  const track = (
    <div
      className={css.effortHit}
      data-testid={tid("slider")}
      role="slider"
      tabIndex={locked ? -1 : 0}
      aria-label="effort"
      aria-valuemin={0}
      aria-valuemax={Math.max(0, stops.length - 1)}
      aria-valuenow={shown}
      aria-valuetext={stopLabel}
      data-index={String(shown)}
      data-tier-index={String(stop?.index ?? 0)}
      data-name={stop?.name ?? ""}
      data-tiers={stops.map((s) => s.name).join(",")}
      data-effort-look={look}
      data-ember={pillEmber ? "1" : "0"}
      data-ultracode={isClaude ? (ultracode ? "1" : "0") : "0"}
      data-disabled={locked ? "1" : "0"}
      aria-disabled={locked}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onKeyDown={onKeyDown}
    >
      <div
        ref={trackRef}
        className={css.effortTrack}
        data-testid={tid("track")}
        style={{ ["--pos" as string]: String(ratio), ["--knob" as string]: `${KNOB_INSET}px` }}
      >
        <span className={css.effortClip} aria-hidden="true">
          <span className={css.effortFill} data-testid={tid("fill")} />
          {pillEmber ? (
            <span className={css.effortEmbers} data-testid={tid("embers")} data-intensity="ultra">
              <span className={css.effortEmberGlow} />
              <span className={`${css.effortEmberLayer} ${css.effortEmberBack}`} />
              <span className={`${css.effortEmberLayer} ${css.effortEmberMid}`} />
              <span className={`${css.effortEmberLayer} ${css.effortEmberFront}`} />
              <span className={`${css.effortEmberLayer} ${css.effortEmberDots}`} />
            </span>
          ) : null}
        </span>
        {stops.map((s, i) => (
          <span
            key={s.name}
            className={[
              css.effortDot,
              i <= shown ? css.effortDotOn : "",
              pillEmber && i <= shown ? css.effortDotEmber : "",
              fallbackIndex === i ? css.effortDotDefault : "",
            ].join(" ")}
            style={{ ["--dot" as string]: String(effortRatio(i, stops.length)) }}
            data-default={fallbackIndex === i ? "1" : undefined}
            title={fallbackIndex === i ? "该模型的默认档" : undefined}
          />
        ))}
        <span
          className={[css.effortKnob, top ? css.effortKnobTop : "", pillEmber ? css.effortKnobUltra : ""].join(" ")}
          data-testid={tid("knob")}
        />
        {pillEmber ? (
          <span className={`${css.effortKnobGlow} ${css.effortKnobGlowUltra}`} aria-hidden="true" />
        ) : null}
      </div>
    </div>
  );

  const ticks = (
    <div className={`${css.effortTicks} ${inline ? "" : css.effortTicksPop}`} data-harness={kind} aria-hidden="true">
      {stops.map((s, i) => (
        <span
          key={s.name}
          className={[
            css.effortTick,
            i === shown ? css.effortTickOn : "",
            i === shown && look === "ultracode"
              ? css.effortTickUltra
              : i === shown && top
                ? css.effortTickTop
                : "",
          ].join(" ")}
          style={{ ["--tick" as string]: String(effortRatio(i, stops.length)) }}
          title={fallbackIndex === i ? `${s.label ?? s.name} · 该模型默认档` : s.description}
        >
          {fallbackIndex === i ? <span className={css.effortTickDefault} data-default="1" /> : null}
          <span className={css.effortTickFull}>{s.label ?? s.name}</span>
          <span className={css.effortTickShort}>{s.short ?? s.label ?? s.name}</span>
        </span>
      ))}
    </div>
  );

  // ── Ultracode switch (Claude only) ────────────────────────────────────
  const ultraNode = switchVisible ? (
    <div
      className={css.ultraRow}
      data-testid={tid("ultracode")}
      data-state={ultracode ? "on" : "off"}
      data-gate={ultraGate}
      data-disabled={switchDisabled ? "1" : "0"}
      data-effective={switchEffective}
      data-reason={ultraBlocked?.reason ?? ""}
    >
      <span className={css.ultraText}>
        <span className={`${css.ultraLabel} ${ultracode ? css.ultraLabelOn : ""}`} data-testid={tid("ultracode-label")}>
          {ULTRACODE_SWITCH_LABEL}
          <span className={css.ultraMarker} data-testid={tid("ultracode-effective")} data-effective={switchEffective}>
            {ultracode ? (switchEffective === "on" ? "" : switchEffective === "off" ? "· 实际关" : "· ?") : ""}
          </span>
        </span>
        <span className={css.ultraDesc} data-testid={tid("ultracode-hint")}>
          {ultraGate === "coupled" ? ULTRACODE_COUPLED_DESC : ULTRACODE_SWITCH_DESC}
        </span>
        {switchLockedReason ? (
          <span className={css.ultraReason} data-testid={tid("ultracode-reason")}>
            {switchLockedReason}
          </span>
        ) : null}
      </span>
      <button
        type="button"
        role="switch"
        className={`${css.ultraSwitch} ${ultracode ? css.ultraSwitchOn : ""}`}
        data-testid={tid("ultracode-switch")}
        aria-checked={ultracode}
        aria-label={`${ULTRACODE_SWITCH_LABEL} 开关`}
        disabled={switchDisabled}
        title={switchLockedReason ?? undefined}
        onClick={() => onUltracodeChange?.(!ultracode)}
      >
        <span className={css.ultraThumb} />
      </button>
    </div>
  ) : null;

  const listBodyRef = useRef<HTMLDivElement>(null);
  /** Focusable rows in the open list (tiers + models), in DOM order. */
  function focusableRows(): HTMLButtonElement[] {
    return Array.from(
      listBodyRef.current?.querySelectorAll<HTMLButtonElement>("button[data-testid^='effort-tier-'], button[data-testid^='model-option-']") ?? [],
    );
  }
  function focusRow(index: number) {
    focusableRows()[index]?.focus();
  }

  const onListKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const rows = focusableRows();
    if (rows.length === 0) return;
    const currentIndex = rows.findIndex((r) => r === document.activeElement);
    if (event.key === "ArrowDown" || event.key === "ArrowRight") {
      event.preventDefault();
      focusRow(currentIndex < 0 ? 0 : Math.min(currentIndex + 1, rows.length - 1));
    } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
      event.preventDefault();
      focusRow(currentIndex < 0 ? 0 : Math.max(currentIndex - 1, 0));
    } else if (event.key === "Home") {
      event.preventDefault();
      focusRow(0);
    } else if (event.key === "End") {
      event.preventDefault();
      focusRow(rows.length - 1);
    } else if (event.key === "Escape") {
      event.preventDefault();
      setTypeValue("");
      setList(false);
      onClose?.();
    }
  };

  function submitTypedId() {
    const id = typeValue.trim();
    if (!id || modelLockedReason) return;
    onModel?.(id);
    setTypeValue("");
    setList(false);
  }

  if (list) {
    return (
      <div className={frame} data-testid={tid("slider-panel")} data-view="list" data-harness={kind}>
        <div className={css.effortListHead}>
          <button
            type="button"
            className={css.effortIconBtn}
            data-testid={tid("list-back")}
            aria-label="返回滑杆"
            onClick={() => setList(false)}
          >
            <BackIcon />
          </button>
          <span className={css.effortListTitle}>档位{hasModelAxis ? "与模型" : ""}</span>
        </div>
        <div
          ref={listBodyRef}
          className={css.effortListBody}
          data-testid={tid("list")}
          role="listbox"
          aria-label={`${stopLabel}，展开档位与模型`}
          onKeyDown={onListKeyDown}
        >
          {catalogDiagnostic ? (
            <div className={css.effortCatalogNote} data-reason={catalogDiagnostic.reason} data-testid={tid("catalog-note")}>
              {catalogDiagnostic.text}
            </div>
          ) : null}
          {stops.map((s, i) => {
            const rowLook = effortLook(kind, s.index, false);
            return (
              <button
                key={s.name}
                type="button"
                className={`${css.effortRow} ${i === shown ? css.effortOn : ""}`}
                data-testid={tid(`tier-${s.name}`)}
                data-selected={i === shown ? "1" : "0"}
                data-effort-look={rowLook}
                data-default={fallbackIndex === i ? "1" : undefined}
                title={s.description}
                disabled={locked}
                tabIndex={-1}
                onClick={() => {
                  setList(false);
                  setDraft(i);
                  emitTier(i);
                }}
              >
                <span className={`${css.radio} ${i === shown ? css.radioOn : ""}`} />
                <span className={css.effortName}>{s.label ?? s.name}</span>
                <span className={css.effortDesc}>{s.description}</span>
              </button>
            );
          })}
          {hasModelAxis && modelList.length ? (
            <>
              <div className={css.effortListTitle}>模型</div>
              {modelList.map((id) => {
                const short = shortModel(id);
                const selected = currentModel === short;
                const pending = modelPendingShort === short;
                return (
                  <button
                    key={id}
                    type="button"
                    className={`${css.effortRow} ${selected ? css.effortOn : ""}`}
                    data-testid={`model-option-${short}`}
                    data-selected={selected ? "1" : "0"}
                    data-model-pending={pending ? (modelPending?.queued ? "queued" : "switching") : "0"}
                    disabled={Boolean(modelLockedReason)}
                    title={modelLockedReason ?? undefined}
                    onClick={() => {
                      if (modelLockedReason) return;
                      onModel?.(id);
                      setList(false);
                    }}
                  >
                    <span className={`${css.radio} ${selected ? css.radioOn : ""}`} />
                    <span className={css.effortName}>{short}</span>
                    {selected && modelSelectionPath === "typed" ? (
                      <span className={css.effortPathTag} data-testid="model-option-path">
                        直输 id
                      </span>
                    ) : null}
                  </button>
                );
              })}
              {onModel ? (
                <div className={css.effortTypeRow}>
                  <input
                    type="text"
                    className={css.effortTypeInput}
                    data-testid={tid("model-type")}
                    placeholder="直接输入模型 id，交由终端裁决"
                    aria-label="直接输入模型 id"
                    disabled={Boolean(modelLockedReason)}
                    title={modelLockedReason ?? undefined}
                    value={typeValue}
                    onChange={(event) => setTypeValue(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") {
                        event.preventDefault();
                        submitTypedId();
                      }
                      if (event.key === "Escape") {
                        event.stopPropagation();
                        setTypeValue("");
                      }
                    }}
                  />
                </div>
              ) : null}
            </>
          ) : null}
        </div>
        {footer ? <div className={css.menuFoot}>{footer}</div> : null}
      </div>
    );
  }

  if (inline) {
    return (
      <div
        className={frame}
        data-testid={tid("slider-panel")}
        data-view="slider"
        data-variant="inline"
        data-harness={kind}
        data-disabled={locked ? "1" : "0"}
        data-effort-look={look}
        data-ultracode={ultracode ? "1" : "0"}
      >
        <div className={css.effortFormRow}>
          <span className={css.effortFormLabel}>{label}</span>
          <span className={css.effortFormMeta}>
            <span
              className={`${css.effortFormName} ${look === "ultracode" ? css.effortTextUltra : top ? css.effortTextTop : ""}`}
              data-testid={tid("title")}
              data-effort-look={look}
            >
              {stopLabel}
            </span>
            <span className={css.effortFormDesc} data-testid={tid("model")} title={stop?.description}>
              {stop?.description ?? ""}
            </span>
          </span>
        </div>
        {track}
        {ticks}
        {ultraNode}
        {footer ? (
          <div className={css.effortFormFoot} data-testid={tid("foot")}>
            {footer}
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <div
      className={frame}
      data-testid={tid("slider-panel")}
      data-view="slider"
      data-harness={kind}
      data-disabled={locked ? "1" : "0"}
      data-effort-look={look}
    >
      <div className={css.effortHead}>
        <span
          className={`${css.effortBolt} ${pillEmber ? css.effortBoltUltra : top ? css.effortBoltTop : ""}`}
          aria-hidden="true"
        >
          <BoltIcon />
        </span>
        <button
          type="button"
          className={`${css.effortTitleBtn} ${pillEmber ? css.effortTitleUltra : top ? css.effortTitleTop : ""}`}
          data-testid={tid("open-list")}
          aria-label={`${stopLabel}，展开档位与模型`}
          aria-expanded={false}
          onClick={() => setList(true)}
        >
          <span className={css.effortTitle} data-testid={tid("title")} data-effort-look={look}>
            {stopLabel}
          </span>
          <span className={css.effortChevron}>
            <ChevronIcon />
          </span>
        </button>
        <button
          type="button"
          className={css.effortIconBtn}
          data-testid={tid("reset")}
          aria-label="复位到默认档"
          disabled={locked || shown === fallback}
          onClick={() => {
            setDraft(fallback);
            emitTier(fallback);
          }}
        >
          <ResetIcon />
        </button>
      </div>
      <div className={css.effortModel} data-testid={tid("model")} title={chipTitle}>
        {chipText}
      </div>
      {track}
      {ticks}
      {ultraNode}
    </div>
  );
}

function BoltIcon() {
  return (
    <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
      <path d="M9 1.6 3.6 8.6h3.7L7 14.4l5.4-7.2H8.6L9 1.6z" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </svg>
  );
}

function ChevronIcon() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true" focusable="false">
      <path d="m6 3.5 5 4.5-5 4.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function ResetIcon() {
  return (
    <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
      <path
        d="M3.2 3.4v3.2h3.2M12.8 12.6V9.4H9.6M12.6 6a5 5 0 0 0-9-1.4L3.2 6M3.4 10a5 5 0 0 0 9 1.4l.4-1.4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  );
}

function BackIcon() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true" focusable="false">
      <path d="M10 3.5 5 8l5 4.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}
