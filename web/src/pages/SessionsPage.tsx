import { useEffect, useState } from "react";
import { SessionList } from "../features/session/SessionList";
import { useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { SpacesPanel } from "../features/spaces/SpacesPanel";
import { spaceStore, type Space } from "../features/spaces/store";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { PageHeader } from "../components/PageHeader";
import ui from "../styles/ui.module.css";
import css from "./SessionsPage.module.css";

export function SessionsPage({ dimmed = false }: { dimmed?: boolean }) {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  const { spaces, active, prefs, instanceId, newHref } = useSpaceWorkbench();
  // Below 1024 the index column folds into the header's 空间 button, which
  // opens the same panel as an overlay (one instance, same testids).
  const [indexOpen, setIndexOpen] = useState(false);

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
            <button
              type="button"
              className={`${ui.btnGhost} ${css.indexOpen}`}
              data-testid="spaces-index-open"
              aria-expanded={indexOpen}
              onClick={() => setIndexOpen((v) => !v)}
            >
              空间
            </button>
          }
        />
      ) : null}
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
          {hub.hosts.length === 0 && hub.instances.length === 0 ? (
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
      </div>
    </div>
  );
}
