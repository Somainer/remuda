import { useRef, type KeyboardEvent } from "react";
import type { SessionView } from "../../lib/viewPref";
import css from "./ViewSwitch.module.css";

const OPTIONS: { id: SessionView; label: string }[] = [
  { id: "tty", label: "终端" },
  { id: "structured", label: "结构" },
];

/**
 * Two-state segmented toggle for 终端 / 结构. Rendered only when the session has
 * a terminal to attach to; structured-only drivers get no switch at all.
 */
export function ViewSwitch({
  value,
  onChange,
}: {
  value: SessionView;
  onChange: (next: SessionView) => void;
}) {
  const rootRef = useRef<HTMLDivElement>(null);

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const keys = ["ArrowLeft", "ArrowUp", "ArrowRight", "ArrowDown", "Home", "End"];
    if (!keys.includes(event.key)) return;
    event.preventDefault();
    const index = OPTIONS.findIndex((o) => o.id === value);
    let next = index;
    if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (index + OPTIONS.length - 1) % OPTIONS.length;
    if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (index + 1) % OPTIONS.length;
    if (event.key === "Home") next = 0;
    if (event.key === "End") next = OPTIONS.length - 1;
    const target = OPTIONS[next];
    if (!target || target.id === value) return;
    onChange(target.id);
    rootRef.current?.querySelector<HTMLButtonElement>(`[data-view-option='${target.id}']`)?.focus();
  };

  return (
    <div
      ref={rootRef}
      className={css.viewSwitch}
      role="radiogroup"
      aria-label="视图"
      data-testid="view-switch"
      data-view={value}
      onKeyDown={onKeyDown}
    >
      {OPTIONS.map((option) => {
        const on = option.id === value;
        return (
          <button
            key={option.id}
            type="button"
            role="radio"
            aria-checked={on}
            tabIndex={on ? 0 : -1}
            data-view-option={option.id}
            data-testid={`view-switch-${option.id}`}
            className={`${css.viewSeg} ${on ? css.viewSegOn : ""}`}
            onClick={() => {
              if (!on) onChange(option.id);
            }}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}
