import { useEffect, useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { hubStore } from "../../lib/store";
import { projectStatus } from "../../lib/status";
import { selectedTab, spaceStore, type Space, type SpacePrefs } from "./store";
import type { Instance } from "../../types/instance";
import { HarnessGlyph } from "./SpacesPanel";
import css from "./spaces.module.css";

export function SpaceTabs({ space, tabs, prefs, instanceId, newHref }: { space?: Space; tabs: Instance[]; prefs: SpacePrefs; instanceId?: string; newHref: string }) {
  const navigate = useNavigate();
  const activeRef = useRef<HTMLButtonElement>(null);
  const newRef = useRef<HTMLAnchorElement>(null);
  const [closing, setClosing] = useState<string[]>([]);
  useEffect(() => { activeRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" }); }, [instanceId]);
  return <div className={css.tabBar}>
    <div className={css.tabs} role="tablist" aria-label={`${space?.name ?? "空间"} 的 agents`} data-testid="space-tabs">
      {tabs.map((instance, index) => <div key={instance.id} className={css.tab} data-active={instance.id === instanceId}>
        <button type="button" role="tab" data-testid="session-tab" data-instance-id={instance.id} aria-selected={instance.id === instanceId} tabIndex={instance.id === instanceId || (!instanceId && index === 0) ? 0 : -1} ref={instance.id === instanceId ? activeRef : undefined} className={css.tabSelect} onClick={() => {
          if (space) spaceStore.selectTab(space.id, instance.id);
          navigate(`/s/${instance.id}`);
        }} onKeyDown={(event) => {
          const next = event.key === "ArrowRight" ? (index + 1) % tabs.length : event.key === "ArrowLeft" ? (index + tabs.length - 1) % tabs.length : event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : -1;
          if (next < 0) return;
          event.preventDefault();
          if (space) spaceStore.selectTab(space.id, tabs[next].id);
          navigate(`/s/${tabs[next].id}`);
          const list = event.currentTarget.closest('[role="tablist"]');
          (list?.querySelectorAll('[role="tab"]')[next] as HTMLElement)?.focus();
        }}><HarnessGlyph kind={instance.kind} /><span className={css.tabTitle}>{hubStore.titleOf(instance.id)}</span><StateDot status={projectStatus(instance)} /></button>
        <button type="button" className={css.tabClose} aria-label={`关闭会话 ${hubStore.titleOf(instance.id)}`} title="关闭会话并移除 tab" disabled={closing.includes(instance.id)} onClick={async (event) => {
          if (!space) return;
          const button = event.currentTarget;
          setClosing((ids) => [...ids, instance.id]);
          try {
            await hubStore.close(instance.id);
            spaceStore.closeTab(space.id, instance.id);
            // Do not navigate away if the user switched projects while close was pending.
            if (window.location.pathname.split("/")[2] === instance.id) {
              const next = selectedTab(space, spaceStore.getSnapshot());
              const restoreFocus = document.activeElement === button;
              navigate(next ? `/s/${next.id}` : "/sessions");
              if (restoreFocus) requestAnimationFrame(() => {
                if ((window.location.pathname.split("/")[2] ?? "") === (next?.id ?? "")) {
                  (next ? activeRef.current : newRef.current)?.focus();
                }
              });
            }
          } catch { hubStore.toast("关闭失败，请重试"); }
          finally { setClosing((ids) => ids.filter((id) => id !== instance.id)); }
        }}>×</button>
      </div>)}
      {!tabs.length ? <span className={css.empty}>此空间还没有打开的 agent</span> : null}
    </div>
    <Link ref={newRef} className={css.newTab} to={newHref} aria-label="新建 agent" title="新建 agent">＋</Link>
    <span className={css.tabHint}>{prefs.selectedSpaceId === space?.id ? space?.name : "Agents"}</span>
  </div>;
}
