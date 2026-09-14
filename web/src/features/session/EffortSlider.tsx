import { useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
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
 * Claude has six stops — low · medium · high (default) · xhigh · max ·
 * ultracode — like the Desktop control. The rightmost stop is not a tier: it
 * selects the `xhigh` tier with the ultracode workflow flag
 * (`{name: "xhigh", ultracode: true}` on the wire) and plays the full ember
 * field. `xhigh`/`max` only carry a restrained static top-tier accent — the
 * animated ember exists nowhere but the ultracode stop (`data-effort-look`).
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
  const modelList = model ? modelsFor(kind, models ?? [model]) : [];

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
      aria-valuetext={stop?.name ?? ""}
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
                {/* Ultracode stop: a fourth, denser dotted drift like the Desktop's glow. */}
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
    <div className={`${css.effortTicks} ${inline ? "" : css.effortTicksPop}`} aria-hidden="true">
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
          >
            {/* Full names on the wide inline field; shorts in the popover and on narrow tracks. */}
            <span className={css.effortTickFull}>{s.name}</span>
            <span className={css.effortTickShort}>{s.short ?? s.name}</span>
          </span>
        );
      })}
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
          {stops.map((s, i) => (
            <button
              key={s.name}
              type="button"
              className={`${css.effortRow} ${i === shown ? css.effortOn : ""}`}
              data-testid={tid(`tier-${s.name}`)}
              data-selected={i === shown ? "1" : "0"}
              data-effort-look={effortLook(kind, s.index, s.ultracode)}
              data-ultracode={s.ultracode ? "1" : "0"}
              title={s.ultracode ? ULTRACODE_HINT : undefined}
              disabled={locked}
              onClick={() => pickStop(i)}
            >
              <span className={`${css.radio} ${i === shown ? css.radioOn : ""}`} />
              <span className={css.effortName}>{s.name}</span>
              <span className={css.effortDesc}>{s.description}</span>
            </button>
          ))}
          {modelList.length ? (
            <>
              <div className={css.effortListTitle}>模型</div>
              {modelList.map((id) => (
                <button
                  key={id}
                  type="button"
                  className={css.effortRow + (shortModel(model) === shortModel(id) ? ` ${css.effortOn}` : "")}
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
    // Layout A: no card. A label row (label · level + description), then the
    // dotted pill spanning the form column with tick labels, then the spec
    // helper in the same muted slot as every other field's helper.
    return (
      <div
        className={frame}
        data-testid={tid("slider-panel")}
        data-view="slider"
        data-variant="inline"
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
              {stop?.name ?? "effort"}
            </span>
            <span className={css.effortFormDesc} data-testid={tid("model")}>
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
          aria-label={`${stop?.name ?? "effort"}，展开档位与模型`}
          aria-expanded={false}
          onClick={() => setList(true)}
        >
          <span
            className={css.effortTitle}
            data-testid={tid("title")}
            data-effort-look={look}
          >
            {stop?.name ?? "effort"}
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
      <div className={css.effortModel} data-testid={tid("model")}>
        {modelLabel || stop?.description || ""}
      </div>
      {track}
      {ticks}
    </div>
  );
}
