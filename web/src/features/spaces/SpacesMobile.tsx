import { useEffect, useRef, useState, type RefObject } from "react";
import { useLocation } from "react-router-dom";
import type { Space, SpacePrefs } from "./store";
import { SpacesPanel } from "./SpacesPanel";
import css from "./spaces.module.css";

/**
 * The Space drawer overlay — one component, same testids, for every compact
 * entry: the /m home "☰" button and the compact session header's current-
 * Space chip (D-040 / D-049), and any future 空间 button. It hosts the same
 * SpacesPanel the desktop index column shows.
 */
export function SpacesDrawer({ spaces, active, prefs, instanceId, onSelect, onClose, openerRef }: {
  spaces: Space[];
  active?: Space;
  prefs: SpacePrefs;
  instanceId?: string;
  onSelect: (space: Space) => void;
  onClose: () => void;
  /** Trigger to return focus to when the drawer closes. */
  openerRef: RefObject<HTMLElement | null>;
}) {
  const drawer = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const trigger = openerRef.current;
    drawer.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => trigger?.focus();
  }, [openerRef]);

  return <div className={css.drawerBackdrop} onClick={onClose}>
    <div className={css.drawer} role="dialog" aria-modal="true" aria-label="空间与会话" data-testid="spaces-drawer" ref={drawer} onClick={(event) => event.stopPropagation()} onKeyDown={(event) => {
      if (event.key === "Escape") { event.preventDefault(); onClose(); }
      if (event.key !== "Tab") return;
      const controls = Array.from(drawer.current?.querySelectorAll<HTMLElement>('button:not(:disabled), a[href], input') ?? []);
      const first = controls[0];
      const last = controls.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }}>
      <button type="button" className={css.drawerClose} aria-label="关闭空间面板" onClick={onClose}>关闭 ×</button>
      <SpacesPanel spaces={spaces} active={active} prefs={prefs} instanceId={instanceId} drawer onNavigate={onClose} onSelect={(space) => { onSelect(space); onClose(); }} />
    </div>
  </div>;
}

/**
 * `strip` is the full horizontally-scrollable chips row used on the workbench
 * index routes. `chip` is the D-040 compact `/s/:id*` fold: one current-space
 * chip in the session header that opens the same drawer — both testids
 * (`spaces-chips` wrapper, `spaces-drawer-open` trigger) are kept on purpose so
 * the chip is the strip folded, not a new affordance.
 */
export function SpacesMobile({ spaces, active, prefs, instanceId, onSelect, variant = "strip" }: { spaces: Space[]; active?: Space; prefs: SpacePrefs; instanceId?: string; onSelect: (space: Space) => void; variant?: "strip" | "chip" }) {
  const [open, setOpen] = useState(false);
  const location = useLocation();
  const route = location.pathname + location.search;
  const [openRoute, setOpenRoute] = useState(route);
  if (openRoute !== route) { setOpenRoute(route); setOpen(false); }
  const opener = useRef<HTMLButtonElement>(null);
  const activeChip = useRef<HTMLButtonElement>(null);
  useEffect(() => { activeChip.current?.scrollIntoView({ block: "nearest", inline: "nearest" }); }, [active?.id]);

  const openerButton = (
    <button
      type="button"
      className={variant === "chip" ? `${css.chip} ${css.chipHeader}` : css.drawerOpen}
      data-testid="spaces-drawer-open"
      aria-label="打开空间面板"
      aria-expanded={open}
      title={variant === "chip" ? active?.name ?? "空间" : undefined}
      onClick={() => setOpen(true)}
      ref={opener}
    >
      {variant === "chip" ? (
        // The clip lives on the label, not the button: the button keeps its
        // 44px ::after hot zone (overflow visible) while a long space name
        // still ellipsises.
        <span className={css.chipText}>☰ {active?.name ?? "空间"}</span>
      ) : (
        "☰"
      )}
    </button>
  );

  return <>
    {variant === "chip" ? (
      <span data-testid="spaces-chips">{openerButton}</span>
    ) : (
      <div className={css.mobileSpaces}>
        {openerButton}
        <div className={css.chips} data-testid="spaces-chips" aria-label="空间">
          {spaces.map((space) => <button type="button" key={space.id} className={css.chip} data-testid="space-chip" aria-pressed={space.id === active?.id} ref={space.id === active?.id ? activeChip : undefined} onClick={() => onSelect(space)}>
            <span className={css.chipText}>{space.name}{space.blockedCount ? ` · ${space.blockedCount} 待处理` : ""}</span>
          </button>)}
        </div>
      </div>
    )}
    {open ? <SpacesDrawer spaces={spaces} active={active} prefs={prefs} instanceId={instanceId} onSelect={onSelect} onClose={() => setOpen(false)} openerRef={opener} /> : null}
  </>;
}
