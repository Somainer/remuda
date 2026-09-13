import { useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";
import type { Space, SpacePrefs } from "./store";
import { SpacesPanel } from "./SpacesPanel";
import css from "./spaces.module.css";

export function SpacesMobile({ spaces, active, prefs, instanceId, onSelect }: { spaces: Space[]; active?: Space; prefs: SpacePrefs; instanceId?: string; onSelect: (space: Space) => void }) {
  const [open, setOpen] = useState(false);
  const location = useLocation();
  const route = location.pathname + location.search;
  const [openRoute, setOpenRoute] = useState(route);
  if (openRoute !== route) { setOpenRoute(route); setOpen(false); }
  const drawer = useRef<HTMLDivElement>(null);
  const opener = useRef<HTMLButtonElement>(null);
  const activeChip = useRef<HTMLButtonElement>(null);
  useEffect(() => { activeChip.current?.scrollIntoView({ block: "nearest", inline: "nearest" }); }, [active?.id]);
  useEffect(() => {
    if (!open) return;
    const trigger = opener.current;
    drawer.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => trigger?.focus();
  }, [open]);
  return <>
    <div className={css.mobileSpaces}>
      <button type="button" className={css.drawerOpen} data-testid="spaces-drawer-open" aria-label="打开空间面板" aria-expanded={open} onClick={() => setOpen(true)} ref={opener}>☰</button>
      <div className={css.chips} data-testid="spaces-chips" aria-label="空间">
        {spaces.map((space) => <button type="button" key={space.id} className={css.chip} data-testid="space-chip" aria-pressed={space.id === active?.id} ref={space.id === active?.id ? activeChip : undefined} onClick={() => onSelect(space)}>{space.name}{space.blockedCount ? ` · ${space.blockedCount} 待处理` : ""}</button>)}
      </div>
    </div>
    {open ? <div className={css.drawerBackdrop} onClick={() => setOpen(false)}>
      <div className={css.drawer} role="dialog" aria-modal="true" aria-label="空间与会话" data-testid="spaces-drawer" ref={drawer} onClick={(event) => event.stopPropagation()} onKeyDown={(event) => {
        if (event.key === "Escape") { event.preventDefault(); setOpen(false); }
        if (event.key !== "Tab") return;
        const controls = Array.from(drawer.current?.querySelectorAll<HTMLElement>('button:not(:disabled), a[href], input') ?? []);
        const first = controls[0];
        const last = controls.at(-1);
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
      }}>
        <button type="button" className={css.drawerClose} aria-label="关闭空间面板" onClick={() => setOpen(false)}>关闭 ×</button>
        <SpacesPanel spaces={spaces} active={active} prefs={prefs} instanceId={instanceId} drawer onNavigate={() => setOpen(false)} onSelect={(space) => { onSelect(space); setOpen(false); }} />
      </div>
    </div> : null}
  </>;
}
