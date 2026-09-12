import { useEffect } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { Bell, Folder, Key, MessageSquare, Monitor, Plus, Settings, Bot } from "lucide-react";
import { isSessionRoute } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { SessionList } from "../features/session/SessionList";
import { InstallBar } from "./InstallBar";
import css from "./Shell.module.css";

export function Shell() {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  const location = useLocation();
  const navigate = useNavigate();
  const pending = hub.interactions.filter((i) => i.state === "pending").length;
  const onSessions = isSessionRoute(location.pathname);
  const onSessionPage = location.pathname.startsWith("/s/");
  const showMobileList = mobile && onSessions && !onSessionPage && location.pathname !== "/sessions/new";

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
    <div className={css.shell} data-compact={mobile ? "1" : "0"}>
      <div className={css.install}>
        <InstallBar />
      </div>
      <nav className={css.rail} aria-label="主导航">
        <Link className={`${css.icon} ${onSessions ? css.iconActive : ""}`} to="/sessions" title="会话">
          <MessageSquare size={18} />
        </Link>
        <Link className={`${css.icon} ${location.pathname.startsWith("/approvals") ? css.iconActive : ""}`} to="/approvals" title="审批">
          <Bell size={18} />
          {pending ? <span className={css.badge}>{pending}</span> : null}
        </Link>
        <Link className={css.icon} to="/sessions/new" title="新建">
          <Plus size={18} />
        </Link>
        <div className={css.more}>
          <Link className={css.icon} to="/hosts" title="主机">
            <Monitor size={18} />
          </Link>
          <Link className={css.icon} to="/projects" title="项目">
            <Folder size={18} />
          </Link>
          <Link className={css.icon} to="/providers" title="Provider">
            <Key size={18} />
          </Link>
          <Link className={css.icon} to="/bots" title="Bot">
            <Bot size={18} />
          </Link>
          <Link className={css.icon} to="/settings" title="设置">
            <Settings size={18} />
          </Link>
        </div>
      </nav>
      {onSessions ? (
        <aside className={`${css.list} ${showMobileList ? css.listMobile : ""}`}>
          <header className={css.header}>
            <strong className={css.grow}>会话</strong>
            <Link to="/sessions/new">＋ 新建</Link>
          </header>
          <SessionList />
        </aside>
      ) : (
        <aside className={css.list} />
      )}
      <main className={`${css.main} ${showMobileList ? css.hideMainOnList : ""}`}>
        <Outlet />
      </main>
      <nav className={css.bar} aria-label="手机底栏">
        <Link className={onSessions && !location.pathname.startsWith("/approvals") ? css.barActive : ""} to="/sessions">
          会话
        </Link>
        <Link className={location.pathname.startsWith("/approvals") ? css.barActive : ""} to="/approvals">
          审批{pending ? `·${pending}` : ""}
        </Link>
        <button type="button" onClick={() => navigate("/sessions/new")}>
          ＋
        </button>
        <Link to="/settings">更多</Link>
      </nav>
      {hub.toast ? <div className={css.toast}>{hub.toast.text}</div> : null}
    </div>
  );
}
