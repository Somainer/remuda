import { useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  clampEffortIndex,
  defaultEffortIndex,
  effortIndexFromClientX,
  effortRatio,
  effortTable,
  isEmberTier,
  keyboardEffortIndex,
  shortModel,
  type EffortKind,
  type EffortSelection,
} from "./effort";
import css from "./session.module.css";

function BoltIcon() {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" focusable="false">
      <path d="M9.2 1.2 3.4 8.7h4.1L6.6 14.8l6.4-8.1H8.8l.4-5.5z" fill="currentColor" />
    </svg>
  );
}

function ResetIcon() {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" focusable="false">
      <path
        d="M3.2 3.2v3.4h3.4M3.6 6.4a5.2 5.2 0 1 0 1.2-3.6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="square"
      />
    </svg>
  );
}

export function EffortSlider({
  kind,
  model,
  index,
  disabled,
  onChange,
}: {
  kind: EffortKind | string;
  model?: string;
  index: number;
  disabled?: boolean;
  onChange: (next: EffortSelection) => void;
}) {
  const table = effortTable(kind);
  const trackRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef(false);
  const [draft, setDraft] = useState<number | null>(null);
  const shown = clampEffortIndex(draft ?? index, table.length);
  const locked = Boolean(disabled) || table.length === 0;
  const ember = isEmberTier(kind, shown);
  const current = table[shown];
  const ratio = effortRatio(shown, table.length);
  const fallback = defaultEffortIndex(kind);
  const modelLabel = model ? shortModel(model) : "";

  if (table.length === 0) return null;

  const snapFromClientX = (clientX: number): number => {
    const track = trackRef.current?.getBoundingClientRect();
    if (!track) return shown;
    return effortIndexFromClientX(clientX, track, table.length);
  };

  const emit = (next: number) => {
    const clamped = Math.max(0, Math.min(next, table.length - 1));
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

  const hint = current ? `${current.name} · ${current.description}` : "";

  return (
    <div className={css.effortSlider} data-testid="effort-slider-panel" data-disabled={locked ? "1" : "0"}>
      <div className={css.effortHead}>
        <span className={`${css.effortBolt} ${ember ? css.effortBoltEmber : ""}`}>
          <BoltIcon />
        </span>
        <div className={css.effortHeadText}>
          <div
            className={`${css.effortTitle} ${ember ? css.effortTitleEmber : ""}`}
            data-testid="effort-title"
            data-ember={ember ? "1" : "0"}
          >
            {current?.name ?? "effort"}
          </div>
          {modelLabel ? (
            <div className={css.effortModel} data-testid="effort-model">
              {modelLabel}
            </div>
          ) : null}
        </div>
        <button
          type="button"
          className={css.effortReset}
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
        aria-valuetext={hint}
        aria-disabled={locked}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onKeyDown={onKeyDown}
      >
        <div ref={trackRef} className={css.effortTrack}>
          <div
            className={`${css.effortFill} ${ember ? css.effortFillEmber : ""}`}
            style={{ width: `${ratio * 100}%` }}
          />
          <span
            className={`${css.effortKnob} ${ember ? css.effortKnobEmber : ""}`}
            data-testid="effort-knob"
            style={{ left: `${ratio * 100}%` }}
          />
        </div>
      </div>
      <div className={css.effortHint} data-testid="effort-hint">
        {hint}
      </div>
    </div>
  );
}
