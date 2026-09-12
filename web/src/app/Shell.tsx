import { useEffect, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { isSessionRoute, MORE_NAV } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { SessionList } from "../features/session/SessionList";
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
  const onSessionPage = location.pathname.startsWith("/s/");
  const onNew = location.pathname === "/sessions/new";
  const showSidebarList = !mobile && onSessionPage;
  const moreActive = MORE_NAV.some((item) => location.pathname.startsWith(item.to));
  const [moreOpen, setMoreOpen] = useState(false);
  const [morePath, setMorePath] = useState(location.pathname);
  if (morePath !== location.pathname) {
    setMorePath(location.pathname);
    setMoreOpen(false);
  }

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
    <div className={css.shell} data-compact={mobile ? "1" : "0"} data-layout={layoutOf(location.pathname)}>
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
        <Link className={css.icon} to="/sessions/new" title="新建">
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
          <SessionList variant="compact" />
        </aside>
      ) : null}
      <main className={css.main}>
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
        <button type="button" onClick={() => navigate("/sessions/new")} aria-label="新建">
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
