import { useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  CLAUDE_ULTRACODE_INDEX,
  clampEffortIndex,
  defaultEffortIndex,
  EFFORT_MENU_FOOTER,
  effortAt,
  effortIndexFromClientX,
  effortRatio,
  effortTable,
  isEmberEffort,
  keyboardEffortIndex,
  modelsFor,
  shortModel,
  supportsUltracode,
  ULTRACODE_HINT,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import css from "./session.module.css";

/**
 * Half the knob, in px — keep in step with `--knob-size` in session.module.css,
 * which reads it back as `--knob`. It is both the knob's radius and the inset
 * its centre travels within, so pointer aim, knob position and the brand fill
 * (which runs to `centre + this`, hiding its cap under the knob) share a scale.
 */
const KNOB_INSET = 18;

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
 * `ultracode` is Claude-only and is NOT a tier: when on, the tier is forced to
 * `xhigh`, the track locks onto that stop, and the chip + ember mark it.
 */
export function EffortSlider({
  kind,
  model,
  models,
  index,
  ultracode = false,
  disabled,
  variant = "popover",
  idPrefix = "effort",
  label = "effort",
  footer = EFFORT_MENU_FOOTER,
  onChange,
  onModel,
}: {
  kind: EffortKind | string;
  /** Undefined when the harness has no model axis (agy), or when the page owns its own model field. */
  model?: string;
  models?: string[];
  index: number;
  /** Claude ultracode workflow toggle; forces the xhigh tier while on. */
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
}) {
  const tid = (suffix: string) => `${idPrefix}-${suffix}`;
  const inline = variant === "inline";
  const frame = inline
    ? `${css.effortForm}`
    : `${css.effortCard}`;
  const table = effortTable(kind);
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef(false);
  const [draft, setDraft] = useState<number | null>(null);
  const [list, setList] = useState(false);

  const supportsUltra = supportsUltracode(kind);
  const ultraOn = supportsUltra && ultracode === true;
  const baseShown = clampEffortIndex(draft ?? index, table.length);
  // Ultracode forces xhigh; the knob never sits on another stop while it is on.
  const shown = ultraOn ? clampEffortIndex(CLAUDE_ULTRACODE_INDEX, table.length) : baseShown;
  const locked = Boolean(disabled) || table.length === 0;
  // The track takes no tier input while ultracode holds it on xhigh.
  const tierLocked = locked || ultraOn;
  const ember = isEmberEffort(kind, shown, ultraOn);
  const current = table[shown];
  const ratio = effortRatio(shown, table.length);
  const fallback = defaultEffortIndex(kind);
  const modelLabel = model ? shortModel(model) : "";
  const modelList = model ? modelsFor(kind, models ?? [model]) : [];

  if (table.length === 0) return null;

  const snapFromClientX = (clientX: number): number => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect) return shown;
    // The knob centre, not the pill edge, is what the pointer aims at.
    return effortIndexFromClientX(
      clientX,
      { left: rect.left + KNOB_INSET, width: Math.max(1, rect.width - KNOB_INSET * 2) },
      table.length,
    );
  };

  const emit = (next: number, nextUltra: boolean) => {
    const clamped = clampEffortIndex(next, table.length);
    if (clamped === index && nextUltra === ultraOn) return;
    onChange(effortAt((kind as EffortKind) || "claude", clamped, supportsUltra && nextUltra));
  };

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (tierLocked) return;
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
    if (!dragRef.current || tierLocked) return;
    setDraft(snapFromClientX(event.clientX));
  };

  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return;
    const next = snapFromClientX(event.clientX);
    dragRef.current = false;
    setDraft(next);
    emit(next, ultraOn);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (tierLocked) return;
    const next = keyboardEffortIndex(shown, event.key, table.length);
    if (next == null) return;
    event.preventDefault();
    setDraft(next);
    emit(next, false);
  };

  const pickTier = (next: number) => {
    setDraft(next);
    emit(next, false);
    setList(false);
  };

  const toggleUltracode = () => {
    if (!supportsUltra || locked) return;
    emit(ultraOn ? shown : CLAUDE_ULTRACODE_INDEX, !ultraOn);
  };

  const ultraChip = (
    <button
      type="button"
      className={`${css.ultraChip} ${ultraOn ? css.ultraChipOn : ""}`}
      data-testid={tid("ultracode")}
      data-on={ultraOn ? "1" : "0"}
      aria-pressed={ultraOn}
      aria-label={ULTRACODE_HINT}
      title={ULTRACODE_HINT}
      disabled={locked}
      onClick={toggleUltracode}
    >
      {ultraOn ? (
        <span className={css.ultraChipSpark} aria-hidden="true">
          <span className={css.emberSpark} />
          <span className={`${css.emberSpark} ${css.emberSpark2}`} />
          <span className={`${css.emberSpark} ${css.emberSpark3}`} />
        </span>
      ) : (
        <span className={css.ultraChipDot} aria-hidden="true" />
      )}
      <span className={css.ultraChipText}>ultracode</span>
    </button>
  );

  const track = (
    <div
      className={css.effortHit}
      data-testid={tid("slider")}
      data-index={String(shown)}
      data-name={current?.name ?? ""}
      data-ember={ember ? "1" : "0"}
      data-ultracode={ultraOn ? "1" : "0"}
      data-tiers={table.map((tier) => tier.name).join(",")}
      role="slider"
      tabIndex={tierLocked ? -1 : 0}
      aria-label="effort"
      aria-valuemin={0}
      aria-valuemax={Math.max(0, table.length - 1)}
      aria-valuenow={shown}
      aria-valuetext={current?.name ?? ""}
      aria-disabled={tierLocked}
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
            className={`${css.effortFill} ${ember ? css.effortFillEmber : ""}`}
            data-testid={tid("fill")}
          >
            {ember ? (
              <span className={css.effortEmbers} data-testid={tid("embers")}>
                <span className={css.effortEmberGlow} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberBack}`} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberMid}`} />
                <span className={`${css.effortEmberLayer} ${css.effortEmberFront}`} />
              </span>
            ) : null}
          </span>
          {table.map((tier, i) => (
            <span
              key={tier.name}
              className={`${css.effortDot} ${i <= shown ? css.effortDotOn : ""}`}
              style={{ ["--dot" as string]: String(effortRatio(i, table.length)) }}
            />
          ))}
        </span>
        {ember ? <span className={css.effortKnobGlow} aria-hidden="true" /> : null}
        <span className={css.effortKnob} data-testid={tid("knob")} />
      </div>
    </div>
  );

  if (list) {
    return (
      <div className={frame} data-testid={tid("slider-panel")} data-view="list">
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
        <div className={css.effortListBody} data-testid={tid("list")}>
          {table.map((tier, i) => (
            <button
              key={tier.name}
              type="button"
              className={`${css.effortRow} ${i === shown ? css.effortOn : ""}`}
              data-testid={tid(`tier-${tier.name}`)}
              data-selected={i === shown ? "1" : "0"}
              data-ember={isEmberEffort(kind, i, false) ? "1" : "0"}
              disabled={locked}
              onClick={() => pickTier(i)}
            >
              <span className={`${css.radio} ${i === shown ? css.radioOn : ""}`} />
              <span className={css.effortName}>{tier.name}</span>
              <span className={css.effortDesc}>{tier.description}</span>
            </button>
          ))}
          {modelList.length ? (
            <>
              <div className={css.effortListTitle}>模型</div>
              {modelList.map((id) => (
                <button
                  key={id}
                  type="button"
                  className={`${css.effortRow} ${shortModel(model) === shortModel(id) ? css.effortOn : ""}`}
                  data-testid={`model-option-${shortModel(id)}`}
                  onClick={() => {
                    onModel?.(id);
                    setList(false);
                  }}
                >
                  <span
                    className={`${css.radio} ${shortModel(model) === shortModel(id) ? css.radioOn : ""}`}
                  />
                  <span className={css.effortName}>{shortModel(id)}</span>
                </button>
              ))}
            </>
          ) : null}
        </div>
        {footer ? <div className={css.menuFoot}>{footer}</div> : null}
      </div>
    );
  }

  if (inline) {
    // Layout A: no card. A label row (label · level + description · ultracode),
    // then the dotted pill spanning the form column with tick labels, then the
    // spec helper in the same muted slot as every other field's helper.
    return (
      <div
        className={frame}
        data-testid={tid("slider-panel")}
        data-view="slider"
        data-variant="inline"
        data-disabled={locked ? "1" : "0"}
        data-ultracode={ultraOn ? "1" : "0"}
      >
        <div className={css.effortFormRow}>
          <span className={css.effortFormLabel}>{label}</span>
          <span className={css.effortFormMeta}>
            <span className={css.effortFormName} data-testid={tid("title")} data-ember={ember ? "1" : "0"}>
              {current?.name ?? "effort"}
            </span>
            <span className={css.effortFormDesc} data-testid={tid("model")}>
              {current?.description ?? ""}
            </span>
          </span>
          {supportsUltra ? ultraChip : null}
        </div>
        {track}
        <div className={css.effortTicks} aria-hidden="true">
          {table.map((tier, i) => (
            <span
              key={tier.name}
              className={`${css.effortTick} ${i === shown ? css.effortTickOn : ""} ${ember && i === shown ? css.effortTickEmber : ""}`}
              style={{ ["--tick" as string]: String(effortRatio(i, table.length)) }}
            >
              <span className={css.effortTickFull}>{tier.name}</span>
            </span>
          ))}
        </div>
        {footer ? (
          <div className={css.effortFormFoot} data-testid={tid("foot")}>
            {footer}
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <div className={frame} data-testid={tid("slider-panel")} data-view="slider" data-disabled={locked ? "1" : "0"}>
      <div className={css.effortHead}>
        <span className={`${css.effortBolt} ${ember ? css.effortBoltEmber : ""}`} aria-hidden="true">
          <BoltIcon />
        </span>
        <button
          type="button"
          className={`${css.effortTitleBtn} ${ember ? css.effortTitleEmber : ""}`}
          data-testid={tid("open-list")}
          aria-label={`${current?.name ?? "effort"}，展开档位与模型`}
          aria-expanded={false}
          onClick={() => setList(true)}
        >
          <span className={css.effortTitle} data-testid={tid("title")} data-ember={ember ? "1" : "0"}>
            {current?.name ?? "effort"}
          </span>
          <span className={css.effortChevron}>
            <ChevronIcon />
          </span>
        </button>
        {supportsUltra ? <span className={css.effortHeadUltra}>{ultraChip}</span> : null}
        <button
          type="button"
          className={css.effortIconBtn}
          data-testid={tid("reset")}
          aria-label="复位到默认档"
          disabled={locked || (shown === fallback && !ultraOn)}
          onClick={() => {
            setDraft(fallback);
            emit(fallback, false);
          }}
        >
          <ResetIcon />
        </button>
      </div>
      <div className={css.effortModel} data-testid={tid("model")}>
        {modelLabel || current?.description || ""}
      </div>
      {track}
    </div>
  );
}
