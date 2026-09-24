import { useEffect } from "react";
import { Outlet, useLocation } from "react-router-dom";
import { hubStore, useHub } from "../lib/store";
import { toastAdapter } from "../lib/notify";
import { useWorkbenchViewport } from "../lib/viewport";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { InstallBar } from "./InstallBar";
import { PhoneNav } from "./PhoneNav";
import { ShellNotify } from "./Shell";
import css from "./phoneShell.module.css";

/**
 * Compact-only shell for the `/m*` phone-first route tree (D-049, ui-spec
 * §4.7). Desktop never mounts it: the router's viewport gate redirects `/m*`
 * to `/sessions` before this element renders.
 *
 * UO-3: home-level chrome is exactly one 52px page header (owned by the
 * mounted home/inbox surface) plus the 56px + safe-area PhoneNav. The old
 * spaces chips strip and SpaceTabs row are gone from `/m` — Space switching
 * opens the shared SpacesDrawer from the home header, and session bodies stay
 * on the shared `/s/:id` routes under `Shell` (no second transcript).
 */
export function PhoneShell() {
  const hub = useHub();
  // E1: reuse the shared visualViewport wiring (it also drives
  // --workbench-height); never compute heights from window.innerHeight here.
  useWorkbenchViewport();
  const location = useLocation();
  const workbench = useSpaceWorkbench();
  const onHome = location.pathname === "/m";
  // Badge rule identical to Shell.tsx: pending interactions only.
  const pending = hub.interactions.filter((i) => i.state === "pending").length;

  // Equivalent to Shell.tsx's visibilitychange effect: on return to the
  // foreground a session route catches up its journal gap, every other route
  // (both /m pages included) does a full refresh. Do not drop this when
  // editing the shell.
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

  // Bridge legacy hubStore.toast(text) callers onto notify(), mirroring
  // Shell.tsx: /m pages need the same transient surface while Shell is
  // unmounted, and the toast must not sit unconsumed until the next desktop
  // route.
  useEffect(() => {
    if (!hub.toast) return;
    toastAdapter(hub.toast.text, `legacy-toast:${hub.toast.text}`);
    hubStore.clearToast();
  }, [hub.toast]);

  return (
    <div className={css.shell} data-compact="1">
      <div className={css.install}>
        <InstallBar />
      </div>
      <main className={css.main}>
        <Outlet />
      </main>
      <PhoneNav pending={pending} newHref={onHome ? workbench.newHref : "/sessions/new"} />
      <ShellNotify />
    </div>
  );
}
