import { useEffect, useRef, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import {
  Boxes,
  Bot,
  Ellipsis,
  Folder,
  Globe,
  Inbox,
  MessagesSquare,
  Network,
  PanelLeftClose,
  PanelLeftOpen,
  Plug,
  Search,
  Server,
  Settings,
  SquareKanban,
  SquarePen,
  type LucideIcon,
} from "lucide-react";
import { ADMIN_NAV, PRIMARY_NAV, isSessionRoute, isUnder } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { formatDiagnostic, notify, notifyStore, toastAdapter, useLiveAnnouncement, useNotifications, type Notification, type NotifyInput } from "../lib/notify";
import { useWorkbenchViewport } from "../lib/viewport";
import { SpacesMobile } from "../features/spaces/SpacesMobile";
import { SpaceTabs } from "../features/spaces/SpaceTabs";
import { spaceStore } from "../features/spaces/store";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { QuickFind, QUICKFIND_HINT, openQuickFind } from "../features/search/QuickFind";
import { projectFilterStore, useProjectFilter, useProjects, type Project } from "../features/tasks/ProjectSwitcher";
import { SessionsPage } from "../pages/SessionsPage";
import { Icon } from "../components/Icon";
import { InstallBar } from "./InstallBar";
import { PhoneNav } from "./PhoneNav";
import { useWorkbenchKeys } from "./useWorkbenchKeys";
import { AnnotationProvider } from "../features/tasks/AnnotationPanel";
import { AnnotationCapture } from "../features/session/AnnotationCapture";
import { CommitProbe } from "../components/CommitProbe";
import ui from "../styles/ui.module.css";
import css from "./Shell.module.css";
import notifyCss from "./shellNotify.module.css";

/** The router matches `/sessions/` as `/sessions`; route checks here must too. */
function routePath(pathname: string): string {
  return pathname.replace(/\/+$/, "") || "/";
}

function layoutOf(pathname: string): "sessions" | "session" | "sheet" | "page" {
  const path = routePath(pathname);
  if (path === "/sessions/new") return "sheet";
  if (path.startsWith("/s/")) return "session";
  if (path === "/sessions") return "sessions";
  return "page";
}

/**
 * Which pieces of app chrome a route gets. The tab strip is a /s/* surface
 * (desktop); the phone home bar is for home-level screens and never renders
 * on /s/* (D-049).
 */
export function shellChrome(pathname: string, mobile: boolean) {
  const session = layoutOf(pathname) === "session";
  return { sidebar: !mobile, tabs: !mobile && session, phoneNav: mobile && !session };
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

const NAV_ICONS: Record<(typeof PRIMARY_NAV)[number]["id"], LucideIcon> = {
  sessions: MessagesSquare,
  inbox: Inbox,
  board: SquareKanban,
};

const ADMIN_ICONS: Record<(typeof ADMIN_NAV)[number]["id"], LucideIcon> = {
  hosts: Server,
  fleet: Network,
  providers: Plug,
  bots: Bot,
  settings: Settings,
};

/**
 * Desktop sidebar (ui-overhaul §4.1). A quiet `--bg-nav` column: brand row,
 * the 主导航 landmark (会话 first), the project scope list, then 新建会话 and
 * the upward 管理 menu. Folded (`prefs.collapsed`, ⌘/Ctrl+B) it keeps icons only.
 */
/** The project a /board URL names ("" for 全局); null off /board. */
function boardScope({ pathname, search }: { pathname: string; search: string }): string | null {
  if (routePath(pathname) !== "/board") return null;
  return new URLSearchParams(search).get("project")?.trim() ?? "";
}

/**
 * Keeps the stored project filter in step with the /board URL, which is the
 * only source of the board scope: Board falls back to the stored filter on a
 * bare /board, so back from ?project=A to /board must clear it to 全局.
 */
export function BoardScopeSync() {
  const location = useLocation();
  const scope = boardScope(location);
  useEffect(() => {
    if (scope === null) return;
    if (scope) projectFilterStore.select(scope);
    else projectFilterStore.clear();
  }, [scope]);
  return null;
}

export function Sidebar({
  collapsed,
  pending,
  newHref,
  projects,
  quickFindOwned,
}: {
  collapsed: boolean;
  pending: number;
  newHref: string;
  projects: readonly Pick<Project, "id" | "name">[];
  /** The /sessions index panel carries its own `quickfind-trigger`. */
  quickFindOwned: boolean;
}) {
  const location = useLocation();
  const navigate = useNavigate();
  const selectedProject = useProjectFilter();
  const [adminOpen, setAdminOpen] = useState(false);
  const [adminPath, setAdminPath] = useState(location.pathname);
  const adminRef = useRef<HTMLDivElement>(null);
  if (adminPath !== location.pathname) {
    setAdminPath(location.pathname);
    setAdminOpen(false);
  }

  useEffect(() => {
    if (!adminOpen) return;
    const onPointer = (event: PointerEvent) => {
      if (!adminRef.current?.contains(event.target as Node)) setAdminOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setAdminOpen(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [adminOpen]);

  const navActive = (id: (typeof PRIMARY_NAV)[number]["id"], to: string) =>
    id === "sessions" ? isSessionRoute(location.pathname) : isUnder(location.pathname, to);
  const adminActive = ADMIN_NAV.some((item) => isUnder(location.pathname, item.to));
  // On /board the URL is the scope (BoardScopeSync); elsewhere the stored one.
  const current = boardScope(location) ?? selectedProject;
  const scope = current && projects.some((p) => p.id === current) ? current : "";

  // ui-spec §1.1: the scope lives in the URL, so a copied link and each
  // history entry keep their project; 全局 is the bare /board.
  function pickProject(id: string) {
    navigate(id ? `/board?project=${encodeURIComponent(id)}` : "/board");
  }

  return (
    <aside className={css.sidebar} data-testid="sidebar" data-collapsed={collapsed} aria-label="侧栏">
      <div className={css.brand}>
        {!collapsed ? (
          <>
            <span className={css.brandName}>Remuda</span>
            <button
              type="button"
              className={ui.iconBtn}
              data-testid={quickFindOwned ? undefined : "quickfind-trigger"}
              aria-label="快速查找会话"
              aria-haspopup="dialog"
              title={`快速查找（${QUICKFIND_HINT}）`}
              onClick={() => openQuickFind()}
            >
              <Icon icon={Search} />
            </button>
          </>
        ) : null}
        <button
          type="button"
          className={ui.iconBtn}
          data-testid="sidebar-toggle"
          aria-label={collapsed ? "展开侧栏" : "折叠侧栏"}
          aria-expanded={!collapsed}
          title="⌘/Ctrl+B"
          onClick={() => spaceStore.setCollapsed(!collapsed)}
        >
          <Icon icon={collapsed ? PanelLeftOpen : PanelLeftClose} />
        </button>
      </div>

      <nav className={css.nav} aria-label="主导航">
        {PRIMARY_NAV.map((item) => {
          const active = navActive(item.id, item.to);
          // D-053: 任务看板 carries the stored scope into the URL, the only
          // scope /board reads (BoardScopeSync); 全局 stays the bare /board.
          const to = item.id === "board" && selectedProject ? `/board?project=${encodeURIComponent(selectedProject)}` : item.to;
          return (
            <Link
              key={item.id}
              to={to}
              className={css.item}
              aria-current={active ? "page" : undefined}
              title={collapsed ? item.label : undefined}
            >
              <Icon icon={NAV_ICONS[item.id]} />
              <span className={css.label}>{item.label}</span>
              {item.id === "inbox" && pending ? (
                <span className={css.count} data-testid="sidebar-inbox-count">
                  {pending}
                </span>
              ) : null}
            </Link>
          );
        })}
      </nav>

      {!collapsed ? (
        <section className={css.projects} aria-label="项目">
          <div className={css.sectionHead}>
            <span>项目</span>
            <Link className={ui.iconBtn} to="/projects" aria-label="全部项目" title="全部项目" data-testid="sidebar-projects-link">
              <Icon icon={Ellipsis} />
            </Link>
          </div>
          <div className={css.projectRows}>
            {[{ id: "", name: "全局" }, ...projects].map((project) => (
              <button
                key={project.id || "global"}
                type="button"
                className={css.projectRow}
                data-testid="sidebar-project-row"
                data-project-id={project.id}
                aria-pressed={project.id === scope}
                title={project.name}
                onClick={() => pickProject(project.id)}
              >
                <Icon icon={project.id ? Folder : Globe} />
                <span className={css.label}>{project.name}</span>
              </button>
            ))}
          </div>
        </section>
      ) : (
        <div className={css.spacer} />
      )}

      <div className={css.foot}>
        <Link className={css.item} to={newHref} title="新建">
          <Icon icon={SquarePen} />
          <span className={css.label}>新建会话</span>
        </Link>
        <div className={css.admin} ref={adminRef}>
          <button
            type="button"
            className={css.item}
            data-testid="sidebar-admin"
            data-active={adminActive ? "1" : undefined}
            aria-haspopup="menu"
            aria-expanded={adminOpen}
            title={collapsed ? "管理" : undefined}
            onClick={() => setAdminOpen((v) => !v)}
          >
            <Icon icon={Boxes} />
            <span className={css.label}>管理</span>
          </button>
          {adminOpen ? (
            <div className={`${ui.menu} ${css.adminMenu}`} role="menu" aria-label="管理">
              {ADMIN_NAV.map((item) => (
                <Link
                  key={item.id}
                  role="menuitem"
                  className={ui.menuItem}
                  aria-current={isUnder(location.pathname, item.to) ? "page" : undefined}
                  to={item.to}
                >
                  <Icon icon={ADMIN_ICONS[item.id]} />
                  <span>{item.label}</span>
                </Link>
              ))}
            </div>
          ) : null}
        </div>
      </div>
    </aside>
  );
}

export function Shell() {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  // D-050 §9: the sidebar's 项目 section scopes the task list and the board
  // (their surfaces read useProjectFilter). Loaded once per shell mount.
  const projectDirectory = useProjects();
  const location = useLocation();
  const pending = hub.interactions.filter((i) => i.state === "pending").length;
  const onSessions = isSessionRoute(location.pathname);
  const layout = layoutOf(location.pathname);
  const onNew = layout === "sheet";
  // Tabs render on /s/* only (desktop). On compact /s/:id* the chips row
  // folds into the session header's current-space chip (D-040 / D-049).
  const onSessionPage = layout === "session";
  const chrome = shellChrome(location.pathname, mobile);
  const workbench = useSpaceWorkbench();
  const activeSpaceId = workbench.active?.id;
  const activeInstanceId = workbench.instanceId;
  const knownInstance = hub.instances.some((i) => i.id === activeInstanceId);
  const collapsed = !mobile && workbench.prefs.collapsed;
  // /sessions (and the dimmed list behind /sessions/new) mounts SpacesPanel,
  // which owns QuickFind there; every other desktop route mounts it here.
  const quickFindOwned = layout === "sessions" || onNew;
  const newHref = onSessions ? workbench.newHref : "/sessions/new";

  useEffect(() => {
    if (activeSpaceId && activeInstanceId && knownInstance) {
      spaceStore.selectTab(activeSpaceId, activeInstanceId);
    }
  }, [activeSpaceId, activeInstanceId, knownInstance]);

  useWorkbenchKeys({ mobile, onSessions, onNew, workbench });

  useEffect(() => {
    const onVis = () => {
      if (document.visibilityState === "visible") {
        const id = location.pathname.startsWith("/s/") ? location.pathname.split("/")[2] : null;
        // D-055: foreground drives the connection machine (immediate reconnect
        // with reset backoff), not a one-shot REST catch-up.
        hubStore.resumeActive(id);
        if (!id) void hubStore.refresh();
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
    <CommitProbe name="Shell">
    <AnnotationProvider>
    <div className={css.shell} data-compact={mobile ? "1" : "0"} data-layout={layout}data-collapsed={collapsed}>
      <BoardScopeSync />
      <div className={css.install}>
        <InstallBar />
      </div>
      {chrome.sidebar ? (
        <Sidebar
          collapsed={collapsed}
          pending={pending}
          newHref={newHref}
          projects={projectDirectory.projects}
          quickFindOwned={quickFindOwned}
        />
      ) : null}
      <main className={css.main}>
        {onSessions && mobile && !onSessionPage ? <SpacesMobile spaces={workbench.spaces} active={workbench.active} prefs={workbench.prefs} instanceId={workbench.instanceId} onSelect={workbench.select} /> : null}
        {/* The tab strip belongs to /s/* only; list routes carry the index
            column instead. D-049: compact /s/:id* renders no strip either —
            the header chip's drawer and Jump To keep every switch. */}
        {chrome.tabs ? <SpaceTabs space={workbench.active} tabs={workbench.tabs} prefs={workbench.prefs} instanceId={workbench.instanceId} newHref={workbench.newHref} /> : null}
        {onNew ? <SessionsPage dimmed /> : null}
        <Outlet />
      </main>
      {/* D-049: compact /s/:id* renders no app bottom bar — that route's
          bottom strip is the collapsed composer (structured) or the terminal
          input/key bars (tty). Back-to-list is the header back link. */}
      {chrome.phoneNav ? <PhoneNav pending={pending} newHref={newHref} /> : null}
      {!mobile && !quickFindOwned ? <QuickFind /> : null}
      <ShellNotify />
      <AnnotationCapture />
    </div>
    </AnnotationProvider>
    </CommitProbe>
  );
}
