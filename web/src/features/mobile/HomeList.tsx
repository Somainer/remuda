import { memo, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { CommitProbe } from "../../components/CommitProbe";
import type { Id } from "../../types/wire";
import type { Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { hubStore } from "../../lib/store";
import type { HubState } from "../../lib/store";
import { uiMode } from "../../lib/status";
import {
  buildSpaces,
  selectedSpace,
  selectedTab,
  spaceStore,
  useSpacesPrefs,
  type Space,
  type SpacePrefs,
} from "../spaces/store";
import { fetchChanges } from "../files/filesApi";
import {
  arrangeHomeGroups,
  buildHomeRows,
  buildHomeTaskLayer,
  homeRowsSignature,
  readHomeOrder,
  writeHomeOrder,
  type HomeGroup,
  type HomeOrder,
  type HomeRow,
} from "./homeRows";
import { TaskGroups, useTaskLedger } from "../tasks/TaskList";
import { SpacesDrawer } from "../spaces/SpacesMobile";
import { ContextRing } from "./ContextRing";
import ui from "../../styles/ui.module.css";
import css from "./home.module.css";

type ResumeState = { busy: boolean; error: string | null };

/**
 * The store slice the home renders (UO-3). Cached by *display* signature: the
 * inbox badge (`interactions` length) flips on its own poll, and a session
 * can accumulate many pending interactions without any row pixel changing —
 * those notifications must return the cached slice so React bails out instead
 * of committing HomeList (`commit:HomeList` perf contract).
 */
type HomeSelection = {
  spaces: Space[];
  instances: Instance[];
  /** Pending interactions only — the single kind the rows ever read. */
  pending: Interaction[];
  rows: ReadonlyMap<string, HomeRow>;
};

/**
 * Single-entry display cache. Keyed by the display SIGNATURE, not the Hub
 * snapshot object: the store replaces its snapshot on every poll (including a
 * poll that only changed the inbox badge), and a snapshot-keyed cache would
 * never hit. A WeakMap keeps the last snapshot referenced so an unchanged
 * store re-notification between our own state changes returns the same value.
 */
let lastSelection: {
  snapshot: HubState;
  prefs: SpacePrefs;
  sig: string;
  value: HomeSelection;
} | null = null;

function selectHome(snapshot: HubState, prefs: SpacePrefs, nowMs: number): HomeSelection {
  // Fast path: same snapshot and prefs (e.g. a re-render from the 30s time
  // clock with no store notification) reuse the previous derivation wholesale.
  if (lastSelection && lastSelection.snapshot === snapshot && lastSelection.prefs === prefs) {
    return lastSelection.value;
  }
  const spaces = buildSpaces(snapshot.workspaces, snapshot.instances, prefs);
  const pending = snapshot.interactions.filter((interaction) => interaction.state === "pending");
  const rows = buildHomeRows({
    spaces,
    interactions: pending,
    titleOf: (instanceId) => hubStore.titleOf(instanceId as Id),
    rollupOf: (instanceId) => hubStore.usageRollupOf(instanceId as Id),
    screenOf: (instanceId) => snapshot.screens[instanceId],
    summaryOf: (instanceId) => hubStore.summaryOf(instanceId as Id),
    eventsOf: (instanceId) => snapshot.events[instanceId],
    nowMs,
  });
  const hostNameOf = (hostId?: string) => hubStore.hostName((hostId ?? "") as Id);
  const sig = homeRowsSignature(
    rows,
    pending.map((interaction) => interaction.instanceId),
    spaces,
    hostNameOf,
  );
  // Display-identical notification (the badge flipped, more pending
  // interactions landed on an already-blocked session, a poll echoed
  // unchanged data): return the cached value so useSyncExternalStore sees the
  // identical reference and React never commits HomeList.
  if (lastSelection && lastSelection.prefs === prefs && lastSelection.sig === sig) {
    return lastSelection.value;
  }
  const value: HomeSelection = { spaces, instances: snapshot.instances, pending, rows };
  lastSelection = { snapshot, prefs, sig, value };
  return value;
}

/** Subscribe to both the Hub and the local Space prefs, cached per display sig. */
function useHomeSelection(nowMs: number): HomeSelection {
  const prefs = useSpacesPrefs();
  return useSyncExternalStore(
    (onStoreChange) => {
      const offHub = hubStore.subscribe(onStoreChange);
      const offSpace = spaceStore.subscribe(onStoreChange);
      return () => {
        offHub();
        offSpace();
      };
    },
    // nowMs is React state captured per render, never Date.now() inside the
    // reader: repeated getSnapshot() calls between store notifications must
    // return the identical reference (useSyncExternalStore caching contract).
    () => selectHome(hubStore.getSnapshot(), prefs, nowMs),
    () => selectHome(hubStore.getSnapshot(), prefs, nowMs),
  );
}

/**
 * The phone home list mounted at `/m` (D-049, ui-spec §4.7).
 *
 * UO-3 chrome: one sticky head — a 52px title row (「会话」 + the Space
 * button that opens the shared SpacesDrawer) over a 52px search/ordering row
 * (44px controls on a coarse pointer) — and no spaces chips strip or tab row.
 * Rows are grouped by project + git branch; each row is the §2.1/D-038 shape
 * (status dot + title + one next-step sentence, error text in the body slot,
 * relative time, remaining-context ring). All derivation lives in the pure
 * homeRows.ts — this component only wires the store slice, per-device
 * ordering, live branch hydration and the one-tap resume path (the same
 * `hubStore.resume` the session page uses, never a second implementation).
 */
/**
 * memo: HomeList takes no props (React Router renders it through the Outlet),
 * so a PhoneShell re-render for the bottom-bar badge never cascades down.
 * Its own updates arrive only through the cached useHomeSelection store
 * reader, which returns the identical slice on a badge-only notification.
 */
export const HomeList = memo(function HomeList() {
  // The broad useHub() subscription is gone on purpose: a badge-only store
  // notification must not reach this component (see HomeSelection).
  // Relative-time labels tick on their own 30s state clock rather than inside
  // the store reader, keeping getSnapshot() referentially stable.
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);
  const selection = useHomeSelection(nowMs);
  const { spaces, instances, pending, rows } = selection;
  const prefs = useSpacesPrefs();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  // The desktop board collapses to /m with its ?project= preserved
  // (resolveLanding keeps the query verbatim); honor it on the task layer.
  const projectFilter = params.get("project");
  const [query, setQuery] = useState("");
  const [order, setOrder] = useState<HomeOrder>(() => readHomeOrder(localStorageAccess()));
  const [branches, setBranches] = useState<Record<string, string>>({});
  const [resumeStates, setResumeStates] = useState<Record<string, ResumeState>>({});
  const [drawerOpen, setDrawerOpen] = useState(false);
  const drawerOpener = useRef<HTMLButtonElement>(null);

  const active = useMemo(() => selectedSpace(spaces, prefs), [spaces, prefs]);

  useEffect(() => {
    writeHomeOrder(order, localStorageAccess());
  }, [order]);

  // Live git branch per project, through the same read-only changes proxy
  // FilesView uses. Unknown/denied/unreachable simply leaves the branch off
  // the header — the group still renders by project name. Candidates are
  // keyed by Space id (not the spaces array identity, which polling recreates
  // every few seconds); the fetch guard lives in a ref and updates apply as
  // long as the component is mounted.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const fetchedBranches = useRef<Set<string>>(new Set());
  const spaceKeys = spaces.map((space) => space.id).join(",");
  const branchCandidates = useMemo(
    () =>
      spaces
        .filter((space) => space.hostId && space.workspaceId)
        .map((space) => ({ id: space.id, hostId: space.hostId!, workspaceId: space.workspaceId! })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [spaceKeys],
  );
  useEffect(() => {
    const targets = branchCandidates.filter(
      (target) => !fetchedBranches.current.has(target.id),
    );
    if (!targets.length) return;
    for (const target of targets) fetchedBranches.current.add(target.id);
    void Promise.all(
      targets.map(async (target) => {
        try {
          const status = await fetchChanges(target.hostId, target.workspaceId);
          const branch = status.branch?.state === "known" ? status.branch.value?.trim() : "";
          return { id: target.id, branch: branch ?? "" };
        } catch {
          return { id: target.id, branch: "" };
        }
      }),
    ).then((results) => {
      if (!mounted.current) return;
      setBranches((current) => {
        const next = { ...current };
        let changed = false;
        for (const { id, branch } of results) {
          if (branch && next[id] !== branch) {
            next[id] = branch;
            changed = true;
          }
        }
        return changed ? next : current;
      });
    });
  }, [branchCandidates]);

  // Mirror SessionList's row hydration cadence exactly: the store projects
  // live phrases for every rendered row, while /screen reads go to
  // tty-attachable rows only (the store additionally skips exited/failed
  // rows, so the phone home issues no screen traffic for dead sessions —
  // c-mobilenew's no-/screen-500 contract).
  const screenIds = useMemo(
    () =>
      instances
        .filter((instance) => instance.parent == null && uiMode(instance) === "tty-attachable")
        .map((instance) => instance.id),
    [instances],
  );
  const topLevelIds = useMemo(
    () => instances.filter((instance) => instance.parent == null).map((instance) => instance.id),
    [instances],
  );
  const screenKey = screenIds.join(",");
  const topKey = topLevelIds.join(",");
  useEffect(() => {
    const tick = () => {
      if (screenIds.length) void hubStore.refreshScreens(screenIds as Id[]);
      if (topLevelIds.length) void hubStore.hydrateRowSummaries(topLevelIds as Id[]);
    };
    tick();
    const timer = window.setInterval(tick, 2500);
    return () => window.clearInterval(timer);
    // Identity-stable ids: the selection slice reuses the cached instance list
    // while nothing row-visible changed, and the joined keys are what actually
    // identify the hydration set.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [screenKey, topKey]);

  const groups = useMemo<HomeGroup[]>(
    () =>
      arrangeHomeGroups({
        spaces,
        rows,
        needle: query,
        order,
        titleOf: (instanceId) => hubStore.titleOf(instanceId as Id),
        hostNameOf: (hostId) => hubStore.hostName((hostId ?? "") as Id),
        branchOf: (spaceId) => branches[spaceId] || null,
      }),
    [spaces, rows, query, order, branches],
  );

  // D-050 task layer stacked above the project+branch session groups:
  // 需要你 first, tasks nested by parent, SE-nn keys, archive folded. Tapping
  // a task row opens the shared /s/:id — no second transcript (D-049).
  // The ledger's own 5s poll lives in this wrapper so its commits stay inside
  // the wrapper subtree: a task-only refresh never re-renders HomeList
  // (`commit:HomeList` measures the session list, not its task child).
  function selectSpace(space: Space) {
    // Same navigation useSpaceWorkbench().select uses: the space's remembered
    // tab, else the index (which bounces back to /m under the compact gate).
    spaceStore.selectSpace(space.id);
    const tab = selectedTab(space, prefs);
    navigate(tab ? `/s/${tab.id}` : "/sessions");
  }

  async function onResume(instanceId: string) {
    setResumeStates((current) => ({ ...current, [instanceId]: { busy: true, error: null } }));
    // D-026: resume is a new instance inheriting the native session; on
    // success the row must leave for the new id, on failure it stays put and
    // the Hub's error is shown (the store also surfaces it as a toast).
    const nextId = await hubStore.resume(instanceId as Id, "structured");
    if (nextId) {
      navigate(`/s/${nextId}`);
      return;
    }
    setResumeStates((current) => ({
      ...current,
      [instanceId]: { busy: false, error: "恢复失败，请重试" },
    }));
  }

  return (
    <div className={css.home} data-testid="home-list">
      {/* The probe wraps ONLY the session list surface (head + project
          groups). The D-050 task layer sits outside it: its own 5s ledger
          poll commits the task subtree, never `commit:HomeList`. */}
      <CommitProbe name="HomeList">
        <>
          <header className={css.head}>
            <div className={css.titleRow}>
              <h1 className={css.title}>会话</h1>
              <button
                ref={drawerOpener}
                type="button"
                className={css.spaceBtn}
                data-testid="spaces-drawer-open"
                aria-haspopup="dialog"
                aria-expanded={drawerOpen}
                aria-label="切换空间"
                title={active?.name ?? "空间"}
                onClick={() => setDrawerOpen(true)}
              >
                <span className={css.spaceBtnName}>{active?.name ?? "空间"}</span>
                <span className={css.spaceBtnCaret} aria-hidden="true">
                  ▾
                </span>
              </button>
            </div>
            <div className={css.toolbar}>
              <input
                className={css.search}
                data-testid="home-search"
                type="search"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder="搜索 会话 / 项目 / 主机"
                aria-label="搜索会话、项目或主机"
              />
              <div className={ui.seg} role="group" aria-label="排序方式" data-testid="home-order">
                <button
                  type="button"
                  className={ui.segItem}
                  data-testid="home-order-clock"
                  aria-pressed={order === "clock"}
                  onClick={() => setOrder("clock")}
                >
                  时钟
                </button>
                <button
                  type="button"
                  className={ui.segItem}
                  data-testid="home-order-list"
                  aria-pressed={order === "list"}
                  onClick={() => setOrder("list")}
                >
                  列表
                </button>
              </div>
            </div>
          </header>
          {drawerOpen ? (
            <SpacesDrawer
              spaces={spaces}
              active={active}
              prefs={prefs}
              instanceId={undefined}
              onSelect={selectSpace}
              onClose={() => setDrawerOpen(false)}
              openerRef={drawerOpener}
            />
          ) : null}
          {groups.length > 0 ? (
            <div className={css.groups}>
              {groups.map((group) => (
                <section
                  key={group.id}
                  className={css.group}
                  data-testid="home-group"
                  data-blocked={group.blockedCount}
                >
                  <header className={css.groupHead}>
                    <span className={css.groupProject} title={`${group.project} · ${group.hostName}`}>
                      {group.project}
                    </span>
                    {group.branch ? <span className={css.groupBranch}>{group.branch}</span> : null}
                    <span
                      className={css.groupBlocked}
                      data-zero={group.blockedCount === 0 ? "1" : "0"}
                    >
                      {group.blockedCount} 待处理
                    </span>
                  </header>
                  {group.rows.map((row) => {
                    const resumeState = resumeStates[row.id];
                    return (
                      <article
                        key={row.id}
                        className={css.row}
                        data-testid="home-row"
                        data-status={row.status}
                        data-blocked={row.blocked ? "1" : "0"}
                      >
                        <Link className={css.rowMain} to={`/s/${row.id}`} data-testid="home-row-link">
                          <span className={css.dot}>
                            <StateDot status={row.status} />
                          </span>
                          <span className={css.rowText}>
                            <span className={css.rowTitle} title={row.title}>
                              {row.title}
                            </span>
                          <span
                            className={css.rowBody}
                            data-testid="home-row-body"
                            data-error={row.bodyIsError ? "1" : "0"}
                            title={row.body}
                          >
                            {row.body}
                          </span>
                        </span>
                        <span className={css.rowTime} title={row.updatedAt}>
                          {row.timeLabel}
                        </span>
                        <ContextRing pct={row.contextPct} />
                      </Link>
                      {row.canResume ? (
                        <button
                          type="button"
                          className={`${ui.btn} ${css.resume}`}
                          data-testid="home-resume"
                          disabled={resumeState?.busy === true}
                          onClick={(event) => {
                            event.preventDefault();
                            void onResume(row.id);
                          }}
                        >
                          {resumeState?.busy ? "恢复中…" : "恢复"}
                        </button>
                      ) : null}
                      {resumeState?.error ? (
                        <p className={css.rowError} role="status" data-testid="home-resume-error">
                          {resumeState.error}
                        </p>
                      ) : null}
                    </article>
                  );
                })}
              </section>
            ))}
            </div>
          ) : null}
        </>
      </CommitProbe>
      {/* Outside the probe: its 5s ledger poll never reports commit:HomeList.
          CSS order keeps it under the sticky head and above project groups. */}
      <HomeTaskLayer
        projectFilter={projectFilter}
        instances={instances}
        pending={pending}
        spaces={spaces}
        branches={branches}
        query={query}
        sessionEmpty={groups.length === 0}
      />
    </div>
  );
});

function localStorageAccess(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/**
 * The D-050 task grouping layer of the phone home, isolated as its own
 * component because `useTaskLedger` polls /v1/tasks every 5 s: a task-only
 * refresh commits this wrapper, never the parent HomeList the
 * `commit:HomeList` probe wraps.
 */
function HomeTaskLayer({
  projectFilter,
  instances,
  pending,
  spaces,
  branches,
  query,
  sessionEmpty = false,
}: {
  projectFilter: string | null;
  instances: Instance[];
  pending: Interaction[];
  spaces: Space[];
  branches: Record<string, string>;
  query: string;
  /** True when the session group list rendered nothing: lets this layer own
      the one shared empty state without the parent subscribing to the ledger. */
  sessionEmpty?: boolean;
}) {
  const taskLedger = useTaskLedger(projectFilter);
  const layer = useMemo(
    () =>
      buildHomeTaskLayer({
        tasks: taskLedger.tasks,
        instances,
        interactions: pending,
        spaces,
        projectName: taskLedger.projectName,
        branchOfSpace: (spaceId) => branches[spaceId] || null,
        query,
      }),
    [taskLedger.tasks, taskLedger.projectName, instances, pending, spaces, branches, query],
  );
  if (layer.length === 0) {
    if (!sessionEmpty) return null;
    return (
      <div className={css.empty} data-testid="home-empty">
        {query.trim() ? "没有匹配的会话" : "暂无会话"}
      </div>
    );
  }
  // A plain wrapper (not .groups): it keeps no bottom padding so it flows
  // straight into the project groups below it.
  return (
    <div data-testid="home-tasks" className={css.taskLayer}>
      <TaskGroups groups={layer} variant="phone" />
    </div>
  );
}
