import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { SessionList } from "../features/session/SessionList";
import { useHomeFreshness } from "../features/home/freshness";
import { useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { SpacesPanel } from "../features/spaces/SpacesPanel";
import { spaceStore, type Space } from "../features/spaces/store";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { PageHeader } from "../components/PageHeader";
import { HomeGlance } from "../features/home/HomeGlance";
import { HostStrip } from "../features/home/HostStrip";
import { GLANCE_TARGETS, scrollToFirst } from "../features/home/glanceNav";
import { MainCard } from "../features/home/MainCard";
import { WorkingList } from "../features/home/WorkingList";
import { useMainAside } from "../features/home/useMain";
import ui from "../styles/ui.module.css";
import css from "./SessionsPage.module.css";

/** The URL's `status` list, the same wire form `readConditions` parses. */
function statusValues(params: URLSearchParams): string[] {
  return (params.get("status") ?? "").split(",").filter(Boolean);
}

export function SessionsPage({ dimmed = false }: { dimmed?: boolean }) {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  // home.md §8: the same freshness signal the list uses. `ready` alone is not
  // enough — the offline-reload path publishes ready:true with empty lists
  // before any successful read, and must not be told it has no hosts.
  const freshness = useHomeFreshness();
  const firstSnapshot = freshness.state === "unknown";
  const { spaces, active, prefs, instanceId, newHref } = useSpaceWorkbench();
  // D-061 (home.md §2.2–2.4): ≥1440 adds the right column — Main card with
  // its inline input, then 工作中; 1280–1439 puts the Main card at the top of
  // the main column. Never behind the dimmed /sessions/new sheet (one 问 Main
  // input at a time).
  const mainAside = useMainAside();
  const showMain = !mobile && !dimmed;
  // Below 1024 the index column folds into the header's 空间 button, which
  // opens the same panel as an overlay (one instance, same testids).
  const [indexOpen, setIndexOpen] = useState(false);
  // §5.2: 工作中 on the count row narrows the SessionList to that rank via the
  // list's own URL filter; a second tap removes it again.
  const [params, setParams] = useSearchParams();
  const workingFilter = statusValues(params).includes("working");

  function toggleWorking() {
    const next = new URLSearchParams(params);
    const statuses = statusValues(params);
    const updated = workingFilter
      ? statuses.filter((value) => value !== "working")
      : [...statuses, "working"];
    if (updated.length) next.set("status", updated.join(","));
    else next.delete("status");
    setParams(next, { replace: true });
  }

  useEffect(() => {
    if (!indexOpen) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setIndexOpen(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [indexOpen]);

  // Picking a Space on the list route keeps the list route: the column is an
  // index of this page, not a jump to the Space's last tab.
  function select(space: Space) {
    spaceStore.selectSpace(space.id);
    setIndexOpen(false);
  }

  return (
    <div className={`${css.page} ${dimmed ? css.dimmed : ""}`} data-testid="sessions-empty">
      {!mobile ? (
        <PageHeader
          crumbs={[{ label: "会话" }]}
          title={active?.name ?? "全部空间"}
          actions={
            <>
              <HomeGlance
                placement="desktop"
                workingFilter={workingFilter}
                onNeedYou={() => scrollToFirst(GLANCE_TARGETS.needYouDesktop)}
                onWorking={toggleWorking}
              />
              <button
                type="button"
                className={`${ui.btnGhost} ${css.indexOpen}`}
                data-testid="spaces-index-open"
                aria-expanded={indexOpen}
                onClick={() => setIndexOpen((v) => !v)}
              >
                空间
              </button>
            </>
          }
        />
      ) : null}
      {/* ② 主机条: the 32px band under the PageHeader (home.md §2.2). */}
      {!mobile ? <HostStrip placement="desktop" /> : null}
      <div className={css.body}>
        {!mobile ? (
          <>
            {indexOpen ? <div className={css.scrim} data-testid="spaces-index-scrim" onClick={() => setIndexOpen(false)} /> : null}
            <aside className={css.index} data-open={indexOpen ? "1" : undefined} aria-label="空间">
              <SpacesPanel spaces={spaces} active={active} prefs={prefs} instanceId={instanceId} collapsed={false} onSelect={select} />
            </aside>
          </>
        ) : null}
        <div className={css.list}>
          {/* D-061 (home.md §2.2–2.4): the Main card heads the main column
              at 1280–1439; at ≥1440 it lives in the right column instead. */}
          {showMain && !mainAside ? <MainCard placement="desktop" /> : null}
          {/* home.md §8: no empty state before the first Hub snapshot — the
              list below paints its skeleton until then. */}
          {!firstSnapshot && hub.ready && hub.hosts.length === 0 && hub.instances.length === 0 ? (
            <div className={css.placeholder}>无主机。请添加主机。</div>
          ) : null}
          <SessionList
            key={active?.id ?? "empty"}
            variant="full"
            instances={active?.instances ?? []}
            title={active?.name ?? "会话"}
            newHref={newHref}
            space={active ? { id: active.id, name: active.name, hostId: active.hostId, workspaceId: active.workspaceId } : undefined}
          />
        </div>
        {showMain && mainAside ? (
          <aside className={css.aside} aria-label="Main 与工作中" data-testid="sessions-aside">
            <MainCard placement="desktop" />
            <WorkingList />
          </aside>
        ) : null}
      </div>
    </div>
  );
}

