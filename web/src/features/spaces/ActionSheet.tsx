import { useEffect, useRef } from "react";
import css from "./spaces.module.css";

export type SheetAction = { id: string; label: string; tone?: "primary" | "danger"; onSelect: () => void };

/**
 * A two-or-three choice sheet for decisions that must not be made by a single
 * ambiguous glyph: stopping a session, dismissing a tab, deleting a record.
 */
export function ActionSheet({ title, detail, actions, busy, onClose, testId }: {
  title: string; detail?: string; actions: SheetAction[]; busy?: boolean; onClose: () => void; testId: string;
}) {
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    panel.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => trigger?.focus();
  }, []);
  return <div className={css.sheetBackdrop} role="presentation" onClick={() => { if (!busy) onClose(); }}>
    <div className={css.sheet} role="dialog" aria-modal="true" aria-label={title} data-testid={testId} ref={panel}
      onClick={(event) => event.stopPropagation()} onKeyDown={(event) => {
        if (event.key === "Escape" && !busy) { event.preventDefault(); onClose(); }
        if (event.key !== "Tab") return;
        const controls = Array.from(panel.current?.querySelectorAll<HTMLElement>("button:not(:disabled)") ?? []);
        const first = controls[0];
        const last = controls.at(-1);
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
      }}>
      <p className={css.sheetTitle}>{title}</p>
      {detail ? <p className={css.sheetDetail}>{detail}</p> : null}
      <div className={css.sheetActions}>
        {actions.map((action) => <button key={action.id} type="button" data-testid={action.id} disabled={busy}
          className={`${css.sheetButton} ${action.tone === "danger" ? css.sheetDanger : action.tone === "primary" ? css.sheetPrimary : ""}`}
          onClick={action.onSelect}>{action.label}</button>)}
        <button type="button" className={css.sheetButton} data-testid={`${testId}-cancel`} disabled={busy} onClick={onClose}>取消</button>
      </div>
    </div>
  </div>;
}
