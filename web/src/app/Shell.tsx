import { useEffect, useLayoutEffect, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { isSessionRoute, MORE_NAV } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { SpacesPanel } from "../features/spaces/SpacesPanel";
import { SpacesMobile } from "../features/spaces/SpacesMobile";
import { SpaceTabs } from "../features/spaces/SpaceTabs";
import { spaceStore } from "../features/spaces/store";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { SessionsPage } from "../pages/SessionsPage";
import { InstallBar } from "./InstallBar";
import css from "./Shell.module.css";

function layoutOf(pathname: string): "sessions" | "session" | "sheet" | "page" {
  if (pathname === "/sessions/new") return "sheet";
  if (pathname.startsWith("/s/")) return "session";
  if (pathname === "/sessions") return "sessions";
  return "page";
}

export function Shell() {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  const location = useLocation();
  const navigate = useNavigate();
  const pending = hub.interactions.filter((i) => i.state === "pending").length;
  const onSessions = isSessionRoute(location.pathname);
  const onNew = location.pathname === "/sessions/new";
  const workbench = useSpaceWorkbench();
  const activeSpaceId = workbench.active?.id;
  const activeInstanceId = workbench.instanceId;
  const knownInstance = hub.instances.some((i) => i.id === activeInstanceId);
  const showSidebarList = !mobile && onSessions;
  const moreActive = MORE_NAV.some((item) => location.pathname.startsWith(item.to));
  const [moreOpen, setMoreOpen] = useState(false);
  const [morePath, setMorePath] = useState(location.pathname);
  if (morePath !== location.pathname) {
    setMorePath(location.pathname);
    setMoreOpen(false);
  }

  useEffect(() => {
    if (activeSpaceId && activeInstanceId && knownInstance) {
      spaceStore.selectTab(activeSpaceId, activeInstanceId);
    }
  }, [activeSpaceId, activeInstanceId, knownInstance]);

  useLayoutEffect(() => {
    if (mobile || !onSessions || onNew) return;
    const onKey = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.isComposing || event.repeat) return;
      const target = event.target;
      if (target instanceof Element && target.closest('input, textarea, select, [contenteditable="true"], .xterm')) return;
      if (event.key.toLowerCase() === "b") {
        event.preventDefault();
        spaceStore.setCollapsed(!workbench.prefs.collapsed);
      } else if (/^[1-9]$/.test(event.key)) {
        const tab = workbench.tabs[Number(event.key) - 1];
        if (!tab || !workbench.active) return;
        event.preventDefault();
        spaceStore.selectTab(workbench.active.id, tab.id);
        navigate(`/s/${tab.id}`);
      } else if (event.code === "BracketLeft" || event.code === "BracketRight") {
        if (!workbench.spaces.length) return;
        event.preventDefault();
        const index = workbench.spaces.findIndex((s) => s.id === workbench.active?.id);
        const step = event.code === "BracketLeft" ? -1 : 1;
        workbench.select(workbench.spaces[(index + step + workbench.spaces.length) % workbench.spaces.length]);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mobile, onSessions, onNew, workbench, navigate]);

  useEffect(() => {
    const onVis = () => {
      if (document.visibilityState === "visible") {
        const id = location.pathname.startsWith("/s/") ? location.pathname.split("/")[2] : null;
        if (id) void hubStore.catchup(id);
        else void hubStore.refresh();
      }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, [location.pathname]);

  useEffect(() => {
    if (!hub.toast) return;
    const t = window.setTimeout(() => hubStore.clearToast(), 2400);
    return () => window.clearTimeout(t);
  }, [hub.toast]);

  return (
    <div className={css.shell} data-compact={mobile ? "1" : "0"} data-layout={layoutOf(location.pathname)} data-spaces={showSidebarList ? "1" : "0"} data-panel-collapsed={workbench.prefs.collapsed}>
      <div className={css.install}>
        <InstallBar />
      </div>
      <nav className={css.rail} aria-label="主导航">
        <Link className={`${css.icon} ${onSessions && !onNew ? css.iconActive : ""}`} to="/sessions" title="会话">
          ▤
        </Link>
        <Link
          className={`${css.icon} ${location.pathname.startsWith("/approvals") ? css.iconActive : ""}`}
          to="/approvals"
          title="审批"
        >
          ◆
          {pending ? <span className={css.badge}>{pending}</span> : null}
        </Link>
        <Link className={css.icon} to={onSessions ? workbench.newHref : "/sessions/new"} title="新建">
          <span className={css.plusBox}>＋</span>
        </Link>
        <div className={css.more}>
          <button
            type="button"
            className={`${css.icon} ${moreActive ? css.iconActive : ""}`}
            title="更多"
            aria-expanded={moreOpen}
            aria-haspopup="menu"
            onClick={() => setMoreOpen((v) => !v)}
          >
            ⋯
          </button>
        </div>
        <Link className={css.me} to="/settings" title="设置">
          <span className={css.meDot}>me</span>
        </Link>
      </nav>
      {showSidebarList ? (
        <aside className={css.list}>
          <SpacesPanel spaces={workbench.spaces} active={workbench.active} prefs={workbench.prefs} instanceId={workbench.instanceId} collapsed={workbench.prefs.collapsed} onSelect={workbench.select} />
        </aside>
      ) : null}
      <main className={css.main}>
        {onSessions && mobile ? <SpacesMobile spaces={workbench.spaces} active={workbench.active} prefs={workbench.prefs} instanceId={workbench.instanceId} onSelect={workbench.select} /> : null}
        {onSessions ? <SpaceTabs space={workbench.active} tabs={workbench.tabs} prefs={workbench.prefs} instanceId={workbench.instanceId} newHref={workbench.newHref} /> : null}
        {onNew ? <SessionsPage dimmed /> : null}
        <Outlet />
      </main>
      <nav className={css.bar} aria-label="手机底栏">
        <Link className={onSessions && !location.pathname.startsWith("/approvals") ? css.barActive : ""} to="/sessions">
          <span className={css.barGlyph}>▤</span>
          会话
        </Link>
        <Link className={location.pathname.startsWith("/approvals") ? css.barActive : ""} to="/approvals">
          <span className={css.barGlyph}>◆</span>
          {pending ? <span className={css.barBadge}>{pending}</span> : null}
          审批
        </Link>
        <button type="button" onClick={() => navigate(onSessions ? workbench.newHref : "/sessions/new")} aria-label="新建">
          <span className={css.barPlus}>＋</span>
        </button>
        <button
          type="button"
          className={moreActive ? css.barActive : ""}
          aria-expanded={moreOpen}
          aria-haspopup="menu"
          onClick={() => setMoreOpen((v) => !v)}
        >
          <span className={css.barGlyph}>⋯</span>
          更多
        </button>
      </nav>
      {moreOpen ? (
        <div className={css.moreMenu} role="menu">
          {MORE_NAV.map((item) => (
            <Link
              key={item.id}
              role="menuitem"
              className={`${css.moreItem} ${location.pathname.startsWith(item.to) ? css.moreItemActive : ""}`}
              to={item.to}
            >
              {item.label}
            </Link>
          ))}
        </div>
      ) : null}
      {hub.toast ? <div className={css.toast}>{hub.toast.text}</div> : null}
    </div>
  );
}
