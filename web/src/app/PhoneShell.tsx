import { useEffect, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router-dom";
import { MORE_NAV, PHONE_NAV } from "../lib/nav";
import { hubStore, useHub } from "../lib/store";
import { toastAdapter } from "../lib/notify";
import { useWorkbenchViewport } from "../lib/viewport";
import { SpacesMobile } from "../features/spaces/SpacesMobile";
import { SpaceTabs } from "../features/spaces/SpaceTabs";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { InstallBar } from "./InstallBar";
import { ShellNotify } from "./Shell";
import css from "./phoneShell.module.css";

/**
 * Compact-only shell for the `/m*` phone-first route tree (D-049, ui-spec
 * §4.7). Desktop never mounts it: the router's viewport gate redirects `/m*`
 * to `/sessions` before this element renders. It owns only home-level
 * navigation — session bodies stay on the shared `/s/:id` routes under
 * `Shell`, so there is deliberately no second transcript implementation.
 *
 * Until m-home / m-inbox land, the outlet renders the existing SessionsPage
 * list and ApprovalsPage unchanged — no new information architecture yet.
 * The /m interim home keeps the same compact index chrome Shell renders at
 * /sessions today (spaces strip + tabs), so the interim screen is the old
 * list in a new shell, not a redesign.
 */
export function PhoneShell() {
  const hub = useHub();
  // E1: reuse the shared visualViewport wiring (it also drives
  // --workbench-height); never compute heights from window.innerHeight here.
  useWorkbenchViewport();
  const location = useLocation();
  const navigate = useNavigate();
  const workbench = useSpaceWorkbench();
  const onHome = location.pathname === "/m";
  // Badge rule identical to Shell.tsx: pending interactions only.
  const pending = hub.interactions.filter((i) => i.state === "pending").length;
  const [moreOpen, setMoreOpen] = useState(false);
  const [morePath, setMorePath] = useState(location.pathname);
  if (morePath !== location.pathname) {
    setMorePath(location.pathname);
    setMoreOpen(false);
  }

  // Equivalent to Shell.tsx's visibilitychange effect: on return to the
  // foreground a session route catches up its journal gap, every other route
  // (both /m pages included) does a full refresh. Do not drop this when
  // editing the shell.
  useEffect(() => {
    const onVis = () => {
      if (document.visibilityState === "visible") {
        const id = location.pathname.startsWith("/s/") ? location.pathname.split("/")[2] : null;
        // D-055: foreground drives the connection machine, not a raw catch-up.
        hubStore.resumeActive(id);
        if (!id) void hubStore.refresh();
      }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, [location.pathname]);

  // Bridge legacy hubStore.toast(text) callers onto notify(), mirroring
  // Shell.tsx: /m pages need the same transient surface while Shell is
  // unmounted, and the toast must not sit unconsumed until the next desktop
  // route.
  useEffect(() => {
    if (!hub.toast) return;
    toastAdapter(hub.toast.text, `legacy-toast:${hub.toast.text}`);
    hubStore.clearToast();
  }, [hub.toast]);

  // Only the two in-tree destinations have an active state: 新建 leaves for
  // the shared /sessions/new route and 更多 opens an overlay (and MORE_NAV
  // destinations render under Shell, outside this /m* tree), so neither can
  // be active here.
  const isActive = (id: (typeof PHONE_NAV)[number]["id"]) => {
    if (id === "home") return location.pathname === "/m";
    if (id === "inbox") return location.pathname.startsWith("/m/inbox");
    return false;
  };

  return (
    <div className={css.shell} data-compact="1">
      <div className={css.install}>
        <InstallBar />
      </div>
      <main className={css.main}>
        {onHome ? (
          <>
            <SpacesMobile
              spaces={workbench.spaces}
              active={workbench.active}
              prefs={workbench.prefs}
              instanceId={workbench.instanceId}
              onSelect={workbench.select}
            />
            <SpaceTabs
              space={workbench.active}
              tabs={workbench.tabs}
              prefs={workbench.prefs}
              instanceId={workbench.instanceId}
              newHref={workbench.newHref}
            />
          </>
        ) : null}
        <Outlet />
      </main>
      <nav className={css.bar} aria-label="手机底栏">
        {PHONE_NAV.map((item) => {
          if (item.id === "more") {
            return (
              <button
                key={item.id}
                type="button"
                data-testid="phone-nav-more"
                aria-expanded={moreOpen}
                aria-haspopup="menu"
                onClick={() => setMoreOpen((v) => !v)}
              >
                <span className={css.barGlyph}>{item.glyph}</span>
                {item.label}
              </button>
            );
          }
          if (item.id === "new") {
            return (
              <button
                key={item.id}
                type="button"
                data-testid="phone-nav-new"
                aria-label={item.label}
                onClick={() => navigate(onHome ? workbench.newHref : "/sessions/new")}
              >
                <span className={css.barPlus}>{item.glyph}</span>
              </button>
            );
          }
          return (
            <Link
              key={item.id}
              to={item.to}
              data-testid={`phone-nav-${item.id}`}
              className={isActive(item.id) ? css.barActive : ""}
              aria-label={item.id === "inbox" && pending ? `${item.label}(${pending})` : item.label}
            >
              <span className={css.barGlyph}>{item.glyph}</span>
              {item.id === "inbox" && pending ? (
                <span className={css.barBadge} data-testid="phone-inbox-badge">
                  {pending}
                </span>
              ) : null}
              {item.label}
            </Link>
          );
        })}
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
      <ShellNotify />
    </div>
  );
}
