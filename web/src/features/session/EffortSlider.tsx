import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  clampEffortIndex,
  defaultEffortIndex,
  EFFORT_MENU_FOOTER,
  effortAtStop,
  effortIndexFromClientX,
  effortLook,
  effortRatio,
  effortStops,
  effortStopIndex,
  keyboardEffortIndex,
  modelsFor,
  shortModel,
  ULTRACODE_HINT,
  type EffortKind,
  type EffortLook,
  type EffortSelection,
  type EffortStop,
} from "./effort";
import type { ModelCatalogView, ModelSelectionPath } from "./modelEffective";
import css from "./session.module.css";

/**
 * Half the knob, in px — keep in step with `--knob-size` in session.module.css,
 * which reads it back as `--knob`. It is both the knob's radius and the inset
 * its centre travels within, so pointer aim, knob position and the brand fill
 * (which runs to `centre + this`, hiding its cap under the knob) share a scale.
 */
const KNOB_INSET = 18;

/** Order-preserving de-dup on the full id (the picker keeps a 1m variant and
 *  its base id distinct). */
function dedupeModels(ids: string[]): string[] {
  const out: string[] = [];
  for (const id of ids) {
    // Dedup on the short label the rows render by, so e.g. e2e/auto and the
    // current passthrough/auto don't produce two "auto" radio rows.
    const short = shortModel(id);
    if (id && !out.some((existing) => shortModel(existing) === short)) out.push(id);
  }
  return out;
}

function BoltIcon() {
  return (
    <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
      <path
        d="M9 1.6 3.6 8.6h3.7L7 14.4l5.4-7.2H8.6L9 1.6z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function ChevronIcon() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true" focusable="false">
      <path d="m6 3.5 5 4.5-5 4.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

function BackIcon() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true" focusable="false">
      <path d="M10 3.5 5 8l5 4.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

function ResetIcon() {
  return (
    <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
      <path
        d="M3.2 3.4v3.2h3.2M3.7 6.4a5 5 0 1 1 .9 4.4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * The one effort slider. The composer mounts it inside a popover; New Session
 * mounts the same track inline (`variant="inline"`, layout A — no card), so the
 * two surfaces share the snapping, pill and ember field rather than each
 * growing their own tier picker.
 *
 * `idPrefix` renames every `data-testid` it emits (`<prefix>-slider`,
 * `-track`, `-knob`, ...) so two mounts can be addressed apart. The composer
 * keeps the default `effort`, i.e. its ids are unchanged.
 *
 * Claude has six stops — low · medium · high (default) · xhigh · max ·
 * ultracode — like the Desktop control. The rightmost stop is not a tier: it
 * selects the `xhigh` tier with the ultracode workflow flag
 * (`{name: "xhigh", ultracode: true}` on the wire) and plays the full ember
 * field. Codex has six native tiers ending in Max (the same static accent)
 * and Ultra (the same strongest ember field), without a workflow flag.
 */
export type ModelCatalogNote = {
  /** Stable machine reason, carried on data-reason. */
  reason: "host-fallback" | "discovery-env-missing" | "discovery-unanswered";
  /** One-line, human-readable warning. */
  text: string;
};

/**
 * Decide the one-line catalog diagnostic, if any. A list the session's own
 * terminal `/model` would reject (the operator's host-fallback cache, a
 * missing discovery gate, or discovery that never answered) is never
 * presented as the session's own list without a mark.
 */
export function catalogNote(catalog: ModelCatalogView | null | undefined): ModelCatalogNote | null {
  if (!catalog) return null;
  if (catalog.cache?.scope === "host-fallback") {
    const base = catalog.cache.baseUrl ? `（relay ${catalog.cache.baseUrl}）` : "";
    return {
      reason: "host-fallback",
      text: `列表来自主机缓存而非本会话的发现${base}，终端 /model 可能拒绝其中的 id；直接输入会交由终端裁决`,
    };
  }
  if (catalog.source === "gateway-discovery" && catalog.discoveryEnv === false) {
    return {
      reason: "discovery-env-missing",
      text: "本会话环境缺少 CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY，列表可能不是网关实时发现的",
    };
  }
  if (catalog.source !== "gateway-discovery" && catalog.discoveryEnv === true) {
    return {
      reason: "discovery-unanswered",
      text: "网关发现尚未写入本会话缓存（base URL 暂无应答）；列表来自设置/内置别名",
    };
  }
  return null;
}

export function EffortSlider({
  kind,
  model,
  modelRequested,
  models,
  modelEffective,
  modelPending,
  modelSelectionPath,
  modelCatalog,
  modelLockedReason = null,
  index,
  ultracode = false,
  disabled,
  variant = "popover",
  idPrefix = "effort",
  label = "effort",
  footer = EFFORT_MENU_FOOTER,
  onChange,
  onModel,
  onClose,
}: {
  kind: EffortKind | string;
  /** Undefined when the harness has no model axis (agy), or when the page owns its own model field. */
  model?: string;
  /** The durable requested model for the requested-vs-running pair (the
   *  launch spec, or an in-flight switch). Distinct from `model`, which is the
   *  picker's folded selection and follows a terminal `/model`. Both strings
   *  render verbatim. */
  modelRequested?: string | null;
  models?: string[];
  /** Resolved effective model id from read-back (may differ from the alias picked). */
  modelEffective?: string | null;
  /** A model switch in flight. */
  modelPending?: { id: string; queued: boolean } | null;
  /** Whether the last Remuda-applied switch used the session's own list or
   *  typed the id verbatim (read back from the verdict). */
  modelSelectionPath?: ModelSelectionPath | null;
  /** Provenance of the rendered catalog; drives the diagnostic note. */
  modelCatalog?: ModelCatalogView | null;
  /** When set, model rows cannot be clicked (exited / observed-only / no
   *  configure cap) and the string says why via the row title. */
  modelLockedReason?: string | null;
  index: number;
  /** Claude ultracode workflow flag; the rightmost slider stop sets it. */
  ultracode?: boolean;
  disabled?: boolean;
  /** `popover` is the composer's framed menu; `inline` is the frameless New Session form row. */
  variant?: "popover" | "inline";
  idPrefix?: string;
  /** Field label rendered at the start of the inline head row. */
  label?: string;
  /** Caption under the tier list. New Session says what the value is written into. */
  footer?: string;
  onChange: (next: EffortSelection) => void;
  onModel?: (model: string) => void;
  /** Popover close (Escape), so focus can return to the trigger. */
  onClose?: () => void;
}) {
  const tid = (suffix: string) => `${idPrefix}-${suffix}`;
  const inline = variant === "inline";
  const frame = inline ? `${css.effortForm}` : `${css.effortCard}`;
  const stops = effortStops(kind);
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef(false);
  const [draft, setDraft] = useState<number | null>(null);
  const [list, setList] = useState(false);

  const ultraOn = kind === "claude" && ultracode === true;
  const propStop = effortStopIndex(kind, index, ultraOn);
  const shown = clampEffortIndex(draft ?? propStop, Math.max(1, stops.length));
  const stop: EffortStop | undefined = stops[shown];
  const stopLabel = stop?.label ?? stop?.name ?? "effort";
  const locked = Boolean(disabled) || stops.length === 0;
  // Three-level ladder: plain · top (restrained static accent) · ultracode
  // (the only animated ember). See effortLook in effort.ts.
  const look: EffortLook = effortLook(kind, stop?.index ?? 0, stop?.ultracode === true);
  const ember = look === "ultracode";
  const top = look === "top";
  const ultraStop = stop?.ultracode === true;
  const ratio = effortRatio(shown, Math.max(1, stops.length));
  const fallback = effortStopIndex(kind, defaultEffortIndex(kind), false);
  const modelLabel = model ? shortModel(model) : "";
  // The current model is the read-back effective id (an alias resolves to a
  // concrete id); fall back to the requested/instance id until read-back.
  const currentModel = modelEffective || model || "";
  // A discovered catalog is the real `/model` picker list; the builtin aliases
  // are appended only when nothing was discovered (models prop absent).
  const modelList = useMemo(
    () =>
      currentModel
        ? models && models.length
          ? dedupeModels([...models, currentModel])
          : modelsFor(kind, [currentModel])
        : [],
    [currentModel, models, kind],
  );
  const modelPendingShort = modelPending?.id ? shortModel(modelPending.id) : null;
  // The requested half of the pair is the durable launch spec (or an in-flight
  // switch), never the picker fold; when the prop is absent (new-session form,
  // bare test mounts) fall back to the picker value. While a requested switch
  // is in flight the effective id is simply stale, so don't show the pair yet.
  // Raw inequality only: both ids render verbatim, no verdict or id rewriting
  // (owner ruling 2026-09-23).
  const requestedModel = (modelRequested ?? model ?? "").trim();
  const effectiveModel = (modelEffective ?? "").trim();
  const modelDifferent =
    !modelPending &&
    Boolean(requestedModel && effectiveModel && requestedModel !== effectiveModel);
  const catalogDiagnostic = modelList.length ? catalogNote(modelCatalog) : null;

  // ── List-view roving keyboard navigation ──────────────────────────────
  type ListRow =
    | { kind: "tier"; key: string; tierIndex: number; disabled: boolean }
    | { kind: "model"; key: string; id: string; disabled: boolean };
  const listRows: ListRow[] = useMemo(() => {
    const tiers = stops.map((s, i) => ({
      kind: "tier" as const,
      key: `tier:${s.name}`,
      tierIndex: i,
      disabled: locked,
    }));
    const modelsRows = modelList.map((id) => ({
      kind: "model" as const,
      key: `model:${id}`,
      id,
      disabled: Boolean(modelLockedReason),
    }));
    return [...tiers, ...modelsRows];
  }, [stops, modelList, locked, modelLockedReason]);
  const [activeRow, setActiveRow] = useState(0);
  const rowRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const [typeValue, setTypeValue] = useState("");

  const scrollRowIntoView = (node: HTMLButtonElement) => {
    // jsdom does not implement scrollIntoView.
    if (typeof node.scrollIntoView === "function") {
      node.scrollIntoView({ block: "nearest" });
    }
  };

  const focusRow = (index: number) => {
    setActiveRow(index);
    const node = rowRefs.current[index];
    if (node) {
      node.focus();
      // Keeps the focused row inside the scroll body with a tall catalog.
      scrollRowIntoView(node);
    }
  };

  // Focus the current selection (selected tier, else current model) once when
  // the list opens, and scroll it into view.
  useEffect(() => {
    if (!list) return;
    let initial = listRows.findIndex(
      (row) => row.kind === "tier" && row.tierIndex === shown && !row.disabled,
    );
    if (initial < 0 && currentModel) {
      initial = listRows.findIndex(
        (row) =>
          row.kind === "model" &&
          !row.disabled &&
          shortModel(row.id) === shortModel(currentModel),
      );
    }
    if (initial < 0) initial = listRows.findIndex((row) => !row.disabled);
    if (initial < 0) initial = 0;
    setActiveRow(initial);
    const handle =
      typeof requestAnimationFrame === "function"
        ? requestAnimationFrame(() => {
            rowRefs.current[initial]?.focus();
            const node = rowRefs.current[initial];
            if (node) scrollRowIntoView(node);
          })
        : null;
    return () => {
      if (handle != null) cancelAnimationFrame(handle);
    };
    // Re-arm only when the view flips; the row set for a given view is stable.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [list]);

  if (stops.length === 0) return null;

  const snapFromClientX = (clientX: number): number => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect) return shown;
    // The knob centre, not the pill edge, is what the pointer aims at.
    return effortIndexFromClientX(
      clientX,
      { left: rect.left + KNOB_INSET, width: Math.max(1, rect.width - KNOB_INSET * 2) },
      stops.length,
    );
  };

  const emit = (next: number) => {
    const clamped = clampEffortIndex(next, stops.length);
    if (clamped === propStop) return;
    onChange(effortAtStop((kind as EffortKind) || "claude", clamped));
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
    setDraft(snapFromClientX(event.clientX));
  };

  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current || locked) return;
    setDraft(snapFromClientX(event.clientX));
  };

  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return;
    const next = snapFromClientX(event.clientX);
    dragRef.current = false;
    setDraft(next);
    emit(next);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (locked) return;
    const next = keyboardEffortIndex(shown, event.key, stops.length);
    if (next == null) return;
    event.preventDefault();
    setDraft(next);
    emit(next);
  };

  const pickStop = (next: number) => {
    setDraft(next);
    emit(next);
    setList(false);
  };

  const track = (
    <div
      className={css.effortHit}
      data-testid={tid("slider")}
      data-index={String(shown)}
      data-tier-index={String(stop?.index ?? 0)}
      data-name={stop?.name ?? ""}
      data-effort-look={look}
      data-ember={ember ? "1" : "0"}
      data-ultracode={ultraStop ? "1" : "0"}
      data-tiers={stops.map((s) => s.name).join(",")}
      role="slider"
      tabIndex={locked ? -1 : 0}
      aria-label="effort"
      aria-valuemin={0}
      aria-valuemax={Math.max(0, stops.length - 1)}
      aria-valuenow={shown}
      aria-valuetext={stopLabel}
      title={stop?.description}
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
        {/* Clipped layer: the pill's own paint. The knob sits outside it so its shadow shows. */}
        <span className={css.effortClip} aria-hidden="true">
          <span
            className={`${css.effortFill} ${ember ? css.effortFillEmber : ""} ${
              top ? css.effortFillTop : ""
            }`}
            data-testid={tid("fill")}
          >
            {ember ? (
              <span
                className={`${css.effortEmbers} ${css.effortEmbersUltra}`}
                data-testid={tid("embers")}
                data-intensity="ultra"
              >
                <span className={css.effortEmberGlow} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberBack}`} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberMid}`} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberFront}`} />
                {/* Top multi-agent stop: a fourth, denser dotted drift. */}
                <span className={`${css.effortEmberLayer} ${css.effortEmberDots}`} />
              </span>
            ) : null}
          </span>
          {stops.map((s, i) => (
            <span
              key={s.name}
              className={`${css.effortDot} ${i <= shown ? css.effortDotOn : ""} ${
                ember && i <= shown ? css.effortDotEmber : ""
              }`}
              style={{ ["--dot" as string]: String(effortRatio(i, stops.length)) }}
            />
          ))}
        </span>
        {ember ? (
          <span className={`${css.effortKnobGlow} ${css.effortKnobGlowUltra}`} aria-hidden="true" />
        ) : null}
        <span
          className={`${css.effortKnob} ${top ? css.effortKnobTop : ""} ${
            ember ? css.effortKnobUltra : ""
          }`}
          data-testid={tid("knob")}
        />
      </div>
    </div>
  );

  const ticks = (
    <div className={`${css.effortTicks} ${inline ? "" : css.effortTicksPop}`} data-harness={kind} aria-hidden="true">
      {stops.map((s, i) => {
        const stopLook = effortLook(kind, s.index, s.ultracode);
        return (
          <span
            key={s.name}
            className={`${css.effortTick} ${i === shown ? css.effortTickOn : ""} ${
              i === shown && stopLook === "ultracode"
                ? css.effortTickUltra
                : i === shown && stopLook === "top"
                  ? css.effortTickTop
                  : ""
            }`}
            style={{ ["--tick" as string]: String(effortRatio(i, stops.length)) }}
            title={s.description}
          >
            {/* Full names on the wide inline field; shorts in the popover and on narrow tracks. */}
            <span className={css.effortTickFull}>{s.label ?? s.name}</span>
            <span className={css.effortTickShort}>{s.short ?? s.label ?? s.name}</span>
          </span>
        );
      })}
    </div>
  );

  const onListKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const enabled = listRows
      .map((row, index) => (row.disabled ? -1 : index))
      .filter((index) => index >= 0);
    if (enabled.length === 0) return;
    const position = enabled.indexOf(activeRow);
    const step = (delta: number) => {
      event.preventDefault();
      const current = position < 0 ? (delta > 0 ? -1 : enabled.length) : position;
      const next = enabled[Math.min(enabled.length - 1, Math.max(0, current + delta))];
      if (next != null) focusRow(next);
    };
    switch (event.key) {
      case "ArrowDown":
      case "ArrowRight":
        step(1);
        break;
      case "ArrowUp":
      case "ArrowLeft":
        step(-1);
        break;
      case "Home":
        event.preventDefault();
        focusRow(enabled[0]);
        break;
      case "End":
        event.preventDefault();
        focusRow(enabled[enabled.length - 1]);
        break;
      case "Escape":
        event.preventDefault();
        event.stopPropagation();
        setList(false);
        onClose?.();
        break;
      default:
        break;
    }
  };

  const submitTypedId = () => {
    const id = typeValue.trim();
    if (!id || modelLockedReason) return;
    onModel?.(id);
    setTypeValue("");
    setList(false);
  };

  if (list) {
    return (
      <div className={frame} data-testid={tid("slider-panel")} data-view="list" data-harness={kind}
        data-model-current={currentModel ? shortModel(currentModel) : ""}
        data-model-pending={modelPending ? (modelPending.queued ? "queued" : "switching") : "0"}
        data-model-different={modelDifferent ? "1" : "0"}
        data-model-path={modelSelectionPath ?? ""}
        data-catalog-source={modelCatalog?.source ?? ""}
      >
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
          <span className={css.effortListTitle}>档位</span>
        </div>
        <div
          className={css.effortListBody}
          data-testid={tid("list")}
          data-popover-scroll="1"
          role="listbox"
          aria-label="档位与模型"
          onKeyDown={onListKeyDown}
        >
          {catalogDiagnostic ? (
            <div
              className={css.effortCatalogNote}
              data-testid={tid("catalog-note")}
              data-reason={catalogDiagnostic.reason}
              title={
                modelCatalog?.cache?.fetchedAt
                  ? `${catalogDiagnostic.text}（fetchedAt ${modelCatalog.cache.fetchedAt}）`
                  : catalogDiagnostic.text
              }
            >
              {catalogDiagnostic.text}
            </div>
          ) : null}
          {stops.map((s, i) => {
            const rowIndex = i;
            return (
              <button
                key={s.name}
                ref={(node) => {
                  rowRefs.current[rowIndex] = node;
                }}
                type="button"
                className={`${css.effortRow} ${i === shown ? css.effortOn : ""}`}
                data-testid={tid(`tier-${s.name}`)}
                data-selected={i === shown ? "1" : "0"}
                data-effort-look={effortLook(kind, s.index, s.ultracode)}
                data-ultracode={s.ultracode ? "1" : "0"}
                title={s.ultracode ? ULTRACODE_HINT : s.description}
                disabled={locked}
                tabIndex={activeRow === rowIndex ? 0 : -1}
                onFocus={() => setActiveRow(rowIndex)}
                onClick={() => pickStop(i)}
              >
                <span className={`${css.radio} ${i === shown ? css.radioOn : ""}`} />
                <span className={css.effortName}>{s.label ?? s.name}</span>
                <span className={css.effortDesc}>{s.description}</span>
              </button>
            );
          })}
          {modelList.length ? (
            <>
              <div className={css.effortListTitle}>模型</div>
              {modelList.map((id, modelIndex) => {
                const short = shortModel(id);
                const selected = shortModel(currentModel) === short;
                const pending = modelPendingShort === short;
                const rowIndex = stops.length + modelIndex;
                const pathTag =
                  selected && modelSelectionPath
                    ? modelSelectionPath === "typed"
                      ? "直输 id"
                      : "列表内"
                    : null;
                return (
                  <button
                    key={id}
                    ref={(node) => {
                      rowRefs.current[rowIndex] = node;
                    }}
                    type="button"
                    className={css.effortRow + (selected ? ` ${css.effortOn}` : "")}
                    data-testid={`model-option-${short}`}
                    data-selected={selected ? "1" : "0"}
                    data-model-pending={pending ? (modelPending?.queued ? "queued" : "switching") : "0"}
                    data-model-path={selected ? (modelSelectionPath ?? "") : ""}
                    title={modelLockedReason ?? id}
                    disabled={Boolean(modelLockedReason)}
                    aria-disabled={Boolean(modelLockedReason)}
                    tabIndex={activeRow === rowIndex ? 0 : -1}
                    onFocus={() => setActiveRow(rowIndex)}
                    onClick={() => {
                      if (modelLockedReason) return;
                      onModel?.(id);
                      setList(false);
                    }}
                  >
                    <span className={`${css.radio} ${selected ? css.radioOn : ""}`} />
                    <span className={css.effortName}>{short}</span>
                    {pathTag ? (
                      <span className={css.effortPathTag} data-testid="model-option-path">
                        {pathTag}
                      </span>
                    ) : null}
                    {pending ? (
                      <span className={css.effortDesc} data-testid="model-option-pending">
                        {modelPending?.queued ? "排队中" : "切换中"}
                      </span>
                    ) : null}
                  </button>
                );
              })}
              {modelDifferent ? (
                <div className={css.effortDesc} data-testid="model-option-different">
                  请求 {requestedModel} → 实际 {effectiveModel}
                </div>
              ) : null}
              {onModel ? (
                <div className={css.effortTypeRow}>
                  <input
                    type="text"
                    className={css.effortTypeInput}
                    data-testid={tid("model-type")}
                    placeholder="直接输入模型 id，回车交由终端裁决"
                    aria-label="直接输入模型 id"
                    value={typeValue}
                    disabled={Boolean(modelLockedReason)}
                    title={modelLockedReason ?? "未列出的 id：原样发送 /model <id>，由终端裁决"}
                    onChange={(event) => {
                      setTypeValue(event.target.value);
                    }}
                    onKeyDown={(event) => {
                      // The listbox's arrow nav must not hijack typing.
                      if (event.key === "Enter") {
                        event.preventDefault();
                        // During an IME composition Enter confirms the
                        // candidate, not the typed id — do not submit.
                        if (!(event.nativeEvent as unknown as globalThis.KeyboardEvent).isComposing) {
                          submitTypedId();
                        }
                        return;
                      }
                      if (event.key === "Escape") {
                        event.preventDefault();
                        setTypeValue("");
                        setList(false);
                        onClose?.();
                        return;
                      }
                      event.stopPropagation();
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
    // Layout A: no card. A label row (label · level + description), then the
    // dotted pill spanning the form column with tick labels, then the spec
    // helper in the same muted slot as every other field's helper.
    return (
      <div
        className={frame}
        data-testid={tid("slider-panel")}
        data-view="slider"
        data-variant="inline"
        data-harness={kind}
        data-disabled={locked ? "1" : "0"}
        data-effort-look={look}
        data-ultracode={ultraStop ? "1" : "0"}
      >
        <div className={css.effortFormRow}>
          <span className={css.effortFormLabel}>{label}</span>
          <span className={css.effortFormMeta}>
            <span
              className={`${css.effortFormName} ${
                look === "ultracode" ? css.effortTextUltra : top ? css.effortTextTop : ""
              }`}
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
          className={`${css.effortBolt} ${ember ? css.effortBoltUltra : top ? css.effortBoltTop : ""}`}
          aria-hidden="true"
        >
          <BoltIcon />
        </span>
        <button
          type="button"
          className={`${css.effortTitleBtn} ${
            ember ? css.effortTitleUltra : top ? css.effortTitleTop : ""
          }`}
          data-testid={tid("open-list")}
          aria-label={`${stopLabel}，展开档位与模型`}
          aria-expanded={false}
          onClick={() => setList(true)}
        >
          <span
            className={css.effortTitle}
            data-testid={tid("title")}
            data-effort-look={look}
          >
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
            emit(fallback);
          }}
        >
          <ResetIcon />
        </button>
      </div>
      <div
        className={css.effortModel}
        data-testid={tid("model")}
        title={
          // Both full ids verbatim on hover; the chip itself carries the same
          // raw text (CSS ellipsis trims for width, it does not rewrite ids).
          modelDifferent
            ? `请求 ${requestedModel} · 实际 ${effectiveModel}`
            : stop?.description
        }
      >
        {kind === "codex"
          ? stop?.description
          : modelDifferent
            ? `${effectiveModel} ⇐ ${requestedModel}`
            : modelLabel || stop?.description || ""}
      </div>
      {track}
      {ticks}
    </div>
  );
}
