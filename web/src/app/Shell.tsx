import { useEffect, useLayoutEffect, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { isSessionRoute, MORE_NAV } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { formatDiagnostic, notify, notifyStore, toastAdapter, useLiveAnnouncement, useNotifications, type Notification, type NotifyInput } from "../lib/notify";
import { useWorkbenchViewport } from "../lib/viewport";
import { isTypingTarget } from "../lib/keyboardScope";
import { switchSlots } from "../lib/sessionSlots";
import { SpacesPanel } from "../features/spaces/SpacesPanel";
import { SpacesMobile } from "../features/spaces/SpacesMobile";
import { SpaceTabs } from "../features/spaces/SpaceTabs";
import { spaceStore } from "../features/spaces/store";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { ProjectSwitcher, useProjects } from "../features/tasks/ProjectSwitcher";
import { SessionsPage } from "../pages/SessionsPage";
import { InstallBar } from "./InstallBar";
import { AnnotationProvider } from "../features/tasks/AnnotationPanel";
import { AnnotationCapture } from "../features/session/AnnotationCapture";
import css from "./Shell.module.css";
import notifyCss from "./shellNotify.module.css";

function layoutOf(pathname: string): "sessions" | "session" | "sheet" | "page" {
  if (pathname === "/sessions/new") return "sheet";
  if (pathname.startsWith("/s/")) return "session";
  if (pathname === "/sessions") return "sessions";
  return "page";
}

/** Test seam for the notification surfaces; mirrors `window.__ttyLab`. */
type NotifyLabHandle = {
  notify: (input: NotifyInput) => string;
  dismissAllBlocking: () => void;
};

declare global {
  interface Window {
    __notifyLab?: NotifyLabHandle;
  }
}

/**
 * Notification surfaces for the whole app (plan §2).
 *
 * Replaces the 2.4-second plain `div` that used to render `hub.toast`. Three
 * separate concerns, deliberately not merged:
 *
 * - a visually-hidden `role="status"` live region carrying only the latest
 *   debounced one-line confirmation. It never receives transcript text: the
 *   only thing written into it is `Notification.text`, built from
 *   subject/stage/reason. (risk 4)
 * - a visible transient strip for the same confirmations, `aria-hidden` so a
 *   screen reader hears the line once rather than twice;
 * - a standing error area that outlives every later success.
 */
export function ShellNotify() {
  const { info, blocking } = useNotifications();
  const announcement = useLiveAnnouncement(info);
  const [copied, setCopied] = useState<string | null>(null);

  /*
   * Test seam, same shape as `window.__ttyLab` in `tty/TerminalView.tsx`.
   *
   * Only the batches that own the call sites can post a real `blocking`
   * notification today (SpacesPanel's delayed-purge branch, store's resume
   * failure), so without this an e2e could not reach the standing error area
   * through the app at all. It posts notifications; it cannot fabricate
   * backend facts.
   */
  useEffect(() => {
    window.__notifyLab = { notify, dismissAllBlocking: notifyStore.dismissAllBlocking };
    return () => {
      delete window.__notifyLab;
    };
  }, []);

  async function copyDiagnostic(notification: Notification) {
    try {
      // `writeText` rejects when the page lacks clipboard permission (common
      // in a headless browser). Catch it here so a denied copy stays silent
      // rather than surfacing as an unhandled rejection.
      await navigator.clipboard?.writeText(formatDiagnostic(notification));
      setCopied(notification.id);
      setTimeout(() => setCopied(null), 1500);
    } catch {
      // Clipboard denied: say nothing rather than post a second failure on
      // top of the error the user is already looking at.
    }
  }

  return (
    <>
      {/*
        The app's only polite live region. Transcript streaming must never be
        routed here; `Transcript.tsx` sets aria-live="off" on its root so an
        ancestor can never make it announce.
      */}
      <div className={notifyCss.srOnly} role="status" aria-live="polite" aria-atomic="true" data-testid="live-region">
        {announcement}
      </div>

      {blocking.length || info.length ? (
        <div className={notifyCss.stack}>
          {/* Confirmations lay out above the errors, never over them. */}
          {info.length ? (
            <div className={notifyCss.info} data-testid="info-toasts" aria-hidden="true">
              {info.map((n) => (
                <div key={n.id} className={notifyCss.infoItem} data-testid="info-toast">
                  {n.text}
                </div>
              ))}
            </div>
          ) : null}

          {blocking.length ? (
            <div
              className={notifyCss.blocking}
              data-testid="blocking-errors"
              role="region"
              aria-label="需要处理的问题"
            >
              {blocking.map((n) => (
                <div key={n.id} className={notifyCss.blockingItem} data-testid="blocking-error" data-key={n.key}>
                  <div className={notifyCss.blockingHead}>
                    <span className={notifyCss.blockingText}>
                      {n.subject}
                      {n.stage ? ` · ${n.stage}` : ""}
                      {n.reason ? <span className={notifyCss.blockingReason}>{n.reason}</span> : null}
                    </span>
                    <button
                      type="button"
                      className={notifyCss.dismiss}
                      aria-label={`忽略：${n.text}`}
                      data-testid="blocking-dismiss"
                      onClick={() => notifyStore.dismiss(n.id)}
                    >
                      ×
                    </button>
                  </div>
                  {n.actions?.length || n.diagnostic ? (
                    <div className={notifyCss.actions}>
                      {n.actions?.map((action) => (
                        <button
                          key={action.id}
                          type="button"
                          className={notifyCss.action}
                          data-testid={`blocking-action-${action.id}`}
                          onClick={() => void action.run?.()}
                        >
                          {action.label}
                        </button>
                      ))}
                      {n.diagnostic ? (
                        <button
                          type="button"
                          className={notifyCss.action}
                          data-testid="blocking-copy-diagnostic"
                          onClick={() => void copyDiagnostic(n)}
                        >
                          {copied === n.id ? "已复制" : "复制诊断"}
                        </button>
                      ) : null}
                    </div>
                  ) : null}
                </div>
              ))}
            </div>
          ) : null}
        </div>
      ) : null}
    </>
  );
}

export function Shell() {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  // D-050 §9: the top-bar 全局▸project switcher scopes the task list and the
  // board (their surfaces read useProjectFilter). Loaded once per shell mount.
  const projectDirectory = useProjects();
  const location = useLocation();
  const navigate = useNavigate();
  const pending = hub.interactions.filter((i) => i.state === "pending").length;
  const onSessions = isSessionRoute(location.pathname);
  const onNew = location.pathname === "/sessions/new";
  // D-040: on compact /s/:id* the full chips row folds into one current-space
  // chip rendered by the session header; the strip stays on index routes.
  const onSessionPage = layoutOf(location.pathname) === "session";
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
      // Never steal a chord from the composer, a form field or an attached
      // terminal. QuickFind's ⌘K shares this guard, so a digit can never
      // switch tabs while the finder is open and owns the keystroke.
      if (isTypingTarget(event.target)) return;
      if (event.key.toLowerCase() === "b") {
        event.preventDefault();
        spaceStore.setCollapsed(!workbench.prefs.collapsed);
      } else if (/^[1-9]$/.test(event.key)) {
        if (!workbench.active) return;
        // Same ordered list SessionList numbers its badges from: the visible
        // tabs of the active Space, dismissal-filtered, capped at nine.
        const tab = switchSlots(workbench.active, workbench.prefs)[Number(event.key) - 1];
        if (!tab) return;
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

  /*
   * Bridge the legacy `hubStore.toast(text)` callers onto `notify()`.
   *
   * Those callers live in files this batch does not own (`SpacesPanel.tsx:49`,
   * `store.ts:583,619`), so rather than edit them, their text is forwarded
   * here as `info` — the same transient behaviour they have today, now in the
   * new surface with a live region attached.
   *
   * TODO(batch D / C2): the `nodePurge !== "purged"` branch at
   * `SpacesPanel.tsx:49` and the 恢复会话失败 path at `store.ts:619` are
   * genuinely blocking and should call `notify({ severity: "blocking" })`
   * directly, with a `projectDeletion()` row and a diagnostic. While they go
   * through this bridge they still self-dismiss after INFO_TTL_MS.
   */
  useEffect(() => {
    if (!hub.toast) return;
    // Collapse by text so a repeated identical toast updates one line.
    toastAdapter(hub.toast.text, `legacy-toast:${hub.toast.text}`);
    hubStore.clearToast();
  }, [hub.toast]);

  return (
    <AnnotationProvider>
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
        {!mobile ? (
          <Link
            className={`${css.icon} ${location.pathname.startsWith("/board") ? css.iconActive : ""}`}
            to="/board"
            title="任务"
          >
            ▦
          </Link>
        ) : null}
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
        {/* Top-bar project scope: desktop only; compact carries project
            grouping on /m (ui-spec §2.9/§4.7, D-049). */}
        {!mobile ? (
          <div
            style={{
              display: "flex",
              justifyContent: "flex-end",
              alignItems: "center",
              gap: 8,
              padding: "6px 12px",
              borderBottom: "1px solid var(--line)",
            }}
          >
            <ProjectSwitcher projects={projectDirectory.projects} />
          </div>
        ) : null}
        {onSessions && mobile && !onSessionPage ? <SpacesMobile spaces={workbench.spaces} active={workbench.active} prefs={workbench.prefs} instanceId={workbench.instanceId} onSelect={workbench.select} /> : null}
        {/* D-049: on compact /s/:id* the SpaceTabs row does not render — the
            header chip's drawer (spaces-drawer-open) and Jump To keep every
            switching capability. Index routes keep their tab strip. */}
        {onSessions && !(mobile && onSessionPage) ? <SpaceTabs space={workbench.active} tabs={workbench.tabs} prefs={workbench.prefs} instanceId={workbench.instanceId} newHref={workbench.newHref} /> : null}
        {onNew ? <SessionsPage dimmed /> : null}
        <Outlet />
      </main>
      {/* D-049: compact /s/:id* renders no app bottom navigation bar — that
          route's bottom strip is the collapsed composer (structured) or the
          terminal input/key bars (tty). Back-to-list is the header back
          link; desktop and the phone /m tree keep their own bars. */}
      {!(mobile && onSessionPage) ? (
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
      ) : null}
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
      <ShellNotify />
      <AnnotationCapture />
    </div>
    </AnnotationProvider>
  );
}
