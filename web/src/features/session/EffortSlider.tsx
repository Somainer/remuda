import { useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  clampEffortIndex,
  defaultEffortIndex,
  EFFORT_MENU_FOOTER,
  effortIndexFromClientX,
  effortRatio,
  effortTable,
  isEmberTier,
  keyboardEffortIndex,
  modelsFor,
  shortModel,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import css from "./session.module.css";

/** Half the knob, in px. The knob centre travels inside the pill by this much on each side. */
const KNOB_INSET = 22;

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

export function EffortSlider({
  kind,
  model,
  models,
  index,
  disabled,
  onChange,
  onModel,
}: {
  kind: EffortKind | string;
  /** Undefined when the harness has no model axis (agy). */
  model?: string;
  models?: string[];
  index: number;
  disabled?: boolean;
  onChange: (next: EffortSelection) => void;
  onModel?: (model: string) => void;
}) {
  const table = effortTable(kind);
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef(false);
  const [draft, setDraft] = useState<number | null>(null);
  const [list, setList] = useState(false);
  const shown = clampEffortIndex(draft ?? index, table.length);
  const locked = Boolean(disabled) || table.length === 0;
  const ember = isEmberTier(kind, shown);
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

  const emit = (next: number) => {
    const clamped = clampEffortIndex(next, table.length);
    const name = table[clamped]?.name ?? "default";
    if (clamped === index) return;
    onChange({ index: clamped, name, kind: (kind as EffortKind) || "claude" });
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
    const next = keyboardEffortIndex(shown, event.key, table.length);
    if (next == null) return;
    event.preventDefault();
    setDraft(next);
    emit(next);
  };

  const pickTier = (next: number) => {
    setDraft(next);
    emit(next);
    setList(false);
  };

  if (list) {
    return (
      <div className={css.effortCard} data-testid="effort-slider-panel" data-view="list">
        <div className={css.effortListHead}>
          <button
            type="button"
            className={css.effortIconBtn}
            data-testid="effort-list-back"
            aria-label="返回滑杆"
            onClick={() => setList(false)}
          >
            <BackIcon />
          </button>
          <span className={css.effortListTitle}>档位</span>
        </div>
        <div className={css.effortListBody} data-testid="effort-list">
          {table.map((tier, i) => (
            <button
              key={tier.name}
              type="button"
              className={`${css.effortRow} ${i === shown ? css.effortOn : ""}`}
              data-testid={`effort-tier-${tier.name}`}
              data-selected={i === shown ? "1" : "0"}
              data-ember={i === table.length - 1 ? "1" : "0"}
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
        <div className={css.menuFoot}>{EFFORT_MENU_FOOTER}</div>
      </div>
    );
  }

  return (
    <div className={css.effortCard} data-testid="effort-slider-panel" data-view="slider" data-disabled={locked ? "1" : "0"}>
      <div className={css.effortHead}>
        <span className={`${css.effortBolt} ${ember ? css.effortBoltEmber : ""}`} aria-hidden="true">
          <BoltIcon />
        </span>
        <button
          type="button"
          className={`${css.effortTitleBtn} ${ember ? css.effortTitleEmber : ""}`}
          data-testid="effort-open-list"
          aria-label={`${current?.name ?? "effort"}，展开档位与模型`}
          aria-expanded={false}
          onClick={() => setList(true)}
        >
          <span className={css.effortTitle} data-testid="effort-title" data-ember={ember ? "1" : "0"}>
            {current?.name ?? "effort"}
          </span>
          <span className={css.effortChevron}>
            <ChevronIcon />
          </span>
        </button>
        <button
          type="button"
          className={css.effortIconBtn}
          data-testid="effort-reset"
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
      <div className={css.effortModel} data-testid="effort-model">
        {modelLabel || current?.description || ""}
      </div>
      <div
        className={css.effortHit}
        data-testid="effort-slider"
        data-index={String(shown)}
        data-name={current?.name ?? ""}
        data-ember={ember ? "1" : "0"}
        data-tiers={table.map((tier) => tier.name).join(",")}
        role="slider"
        tabIndex={locked ? -1 : 0}
        aria-label="effort"
        aria-valuemin={0}
        aria-valuemax={Math.max(0, table.length - 1)}
        aria-valuenow={shown}
        aria-valuetext={current?.name ?? ""}
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
          data-testid="effort-track"
          style={{ ["--pos" as string]: String(ratio), ["--knob" as string]: `${KNOB_INSET}px` }}
        >
          {/* Clipped layer: the pill's own paint. The knob sits outside it so its shadow shows. */}
          <span className={css.effortClip} aria-hidden="true">
            <span className={`${css.effortFill} ${ember ? css.effortFillEmber : ""}`}>
              {ember ? <span className={css.effortSparkle} /> : null}
            </span>
            {table.map((tier, i) => (
              <span
                key={tier.name}
                className={`${css.effortDot} ${i <= shown ? css.effortDotOn : ""}`}
                style={{ ["--dot" as string]: String(effortRatio(i, table.length)) }}
              />
            ))}
          </span>
          <span className={css.effortKnob} data-testid="effort-knob" />
        </div>
      </div>
    </div>
  );
}
