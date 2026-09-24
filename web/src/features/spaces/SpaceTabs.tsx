import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { hubStore } from "../../lib/store";
import { projectStatus } from "../../lib/status";
import { spaceStore, type Space, type SpacePrefs } from "./store";
import type { Instance } from "../../types/instance";
import { ActionSheet } from "./ActionSheet";
import { HarnessGlyph } from "./SpacesPanel";
import { LaunchedByMark } from "../session/LaunchedBy";
import { useTabSet } from "./useSpaceWorkbench";
import css from "./spaces.module.css";

const LONG_PRESS_MS = 500;
const SWIPE_PX = 48;

type ScrollEdges = { left: boolean; right: boolean };

export function SpaceTabs({ space, tabs, prefs, instanceId, newHref }: { space?: Space; tabs: Instance[]; prefs: SpacePrefs; instanceId?: string; newHref: string }) {
  const navigate = useNavigate();
  // The container metadata (aria-label, per-tab owning Space) is derived from
  // the same snapshot the workbench hook renders `tabs` from, so the strip and
  // close/dismiss records never disagree across a Task's Spaces.
  const tabSet = useTabSet();
  const activeRef = useRef<HTMLButtonElement>(null);
  const newRef = useRef<HTMLAnchorElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [closing, setClosing] = useState<string[]>([]);
  const [sheet, setSheet] = useState<Instance>();
  // Phones have no hover: a long press or a swipe raises the same close intent
  // the desktop × does.
  const [revealed, setRevealed] = useState<string>();
  const touch = useRef<{ id: string; x: number; timer: number } | undefined>(undefined);
  // Edge cues show only while the strip actually overflows in that direction;
  // the active tab is scrolled into view so a cue never hides it.
  const [edges, setEdges] = useState<ScrollEdges>({ left: false, right: false });
  const measureEdges = useCallback(() => {
    const element = scrollRef.current;
    if (!element) return;
    const left = element.scrollLeft > 1;
    const right = element.scrollLeft + element.clientWidth < element.scrollWidth - 1;
    setEdges((current) => (current.left === left && current.right === right ? current : { left, right }));
  }, []);

  // Keep the active tab visible when the route or the set's size changes (a
  // Task container can swap the whole strip). Scroll only then: scrolling on
  // every render would fight the user's own horizontal panning.
  useLayoutEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [instanceId, tabs.length]);
  // Cues track the current scroll position on every render too (titles load
  // in, tabs come and go); setEdges bails when nothing changed.
  useLayoutEffect(measureEdges);
  useEffect(() => {
    const element = scrollRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measureEdges);
    observer.observe(element);
    return () => observer.disconnect();
  }, [measureEdges]);
  useEffect(() => () => { if (touch.current) window.clearTimeout(touch.current.timer); }, []);

  function endTouch() {
    if (touch.current) window.clearTimeout(touch.current.timer);
    touch.current = undefined;
  }

  /** Hides the tab; `stop` additionally ends the run the user asked to stop. */
  async function dismiss(instance: Instance, stop: boolean) {
    const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setClosing((ids) => [...ids, instance.id]);
    try {
      if (stop) await hubStore.close(instance.id);
      // A stopped session is gone for good; a dismissed one may come back when
      // it next needs a human. The record always lands on the session's OWN
      // Space — task containers share one dismissal record per Space.
      spaceStore.closeTab(tabSet.ownerOf(instance), instance.id, !stop);
      setSheet(undefined);
      setRevealed(undefined);
      // Do not navigate away if the user switched containers while close was pending.
      if (window.location.pathname.split("/")[2] === instance.id) {
        // Fall back inside THIS container: the next tab the strip still shows.
        const remaining = tabs.filter((row) => row.id !== instance.id);
        const next = remaining[0];
        const restoreFocus = trigger === null || document.activeElement === trigger || document.activeElement === document.body;
        navigate(next ? `/s/${next.id}` : "/sessions");
        if (restoreFocus) requestAnimationFrame(() => {
          if ((window.location.pathname.split("/")[2] ?? "") === (next?.id ?? "")) {
            (next ? activeRef.current : newRef.current)?.focus();
          }
        });
      }
    } catch { hubStore.toast(stop ? "停止失败，请重试" : "关闭失败，请重试"); }
    finally { setClosing((ids) => ids.filter((id) => id !== instance.id)); }
  }

  const overflow = edges.left || edges.right ? (edges.left && edges.right ? "both" : edges.left ? "left" : "right") : "none";

  return <div className={css.tabBar}>
    <div ref={scrollRef} className={css.tabs} role="tablist" aria-label={tabSet.ariaLabel} data-testid="space-tabs" data-overflow={overflow} onScroll={measureEdges}>
      {tabs.map((instance, index) => {
        const status = projectStatus(instance);
        const title = hubStore.titleOf(instance.id);
        return <div key={instance.id} className={css.tab} data-active={instance.id === instanceId} data-revealed={revealed === instance.id}
          onTouchStart={(event) => {
            const point = event.touches[0];
            endTouch();
            touch.current = { id: instance.id, x: point.clientX, timer: window.setTimeout(() => setRevealed(instance.id), LONG_PRESS_MS) };
          }}
          onTouchMove={(event) => {
            if (touch.current?.id !== instance.id) return;
            if (Math.abs(event.touches[0].clientX - touch.current.x) > SWIPE_PX) { setRevealed(instance.id); endTouch(); }
          }}
          onTouchEnd={endTouch} onTouchCancel={endTouch}>
          <button type="button" role="tab" data-testid="session-tab" data-instance-id={instance.id} aria-selected={instance.id === instanceId} tabIndex={instance.id === instanceId || (!instanceId && index === 0) ? 0 : -1} ref={instance.id === instanceId ? activeRef : undefined} className={css.tabSelect} onClick={() => {
            spaceStore.selectTab(tabSet.ownerOf(instance), instance.id);
            navigate(`/s/${instance.id}`);
          }} onKeyDown={(event) => {
            const next = event.key === "ArrowRight" ? (index + 1) % tabs.length : event.key === "ArrowLeft" ? (index + tabs.length - 1) % tabs.length : event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : -1;
            if (next < 0) return;
            event.preventDefault();
            spaceStore.selectTab(tabSet.ownerOf(tabs[next]), tabs[next].id);
            navigate(`/s/${tabs[next].id}`);
            const list = event.currentTarget.closest('[role="tablist"]');
            (list?.querySelectorAll('[role="tab"]')[next] as HTMLElement)?.focus();
          }}><HarnessGlyph kind={instance.kind} /><span className={css.tabTitle}>{title}</span><LaunchedByMark compact launchedBy={instance.launchedBy} /><StateDot status={status} /></button>
          <button type="button" className={css.tabClose} data-testid="tab-close" aria-label={`关闭标签 ${title}`}
            title={status === "exited" ? "关闭标签" : "关闭标签（可选择是否停止会话）"} disabled={closing.includes(instance.id)}
            onClick={() => {
              // An exited session has nothing left to stop, so its tab just goes.
              if (status === "exited") void dismiss(instance, false);
              else setSheet(instance);
            }}>×</button>
        </div>;
      })}
      {!tabs.length ? <span className={css.empty}>此空间还没有打开的 agent</span> : null}
      {edges.left ? <span className={`${css.edge} ${css.edgeLeft}`} aria-hidden="true" /> : null}
      {edges.right ? <span className={`${css.edge} ${css.edgeRight}`} aria-hidden="true" /> : null}
    </div>
    <Link ref={newRef} className={css.newTab} to={newHref} aria-label="新建 agent" title="新建 agent">＋</Link>
    <span className={css.tabHint}>{prefs.selectedSpaceId === space?.id ? space?.name : "Agents"}</span>
    {sheet ? <ActionSheet testId="tab-close-sheet" title={`关闭「${hubStore.titleOf(sheet.id)}」`}
      detail="会话仍在运行。仅关闭标签不会停止它；它在需要你处理时会重新出现。"
      busy={closing.includes(sheet.id)} onClose={() => setSheet(undefined)}
      actions={[
        { id: "tab-close-stop", label: "停止并关闭", tone: "danger", onSelect: () => void dismiss(sheet, true) },
        { id: "tab-close-keep", label: "仅关闭标签", tone: "primary", onSelect: () => void dismiss(sheet, false) },
      ]} /> : null}
  </div>;
}
