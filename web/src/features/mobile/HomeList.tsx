import { memo, useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { CommitProbe } from "../../components/CommitProbe";
import type { Id } from "../../types/wire";
import type { Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { hubStore } from "../../lib/store";
import { useHeldOrder, type HoldSource } from "../../lib/useHeldOrder";
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
  liveChildParents,
  rankFolds,
  readHomeOrder,
  writeHomeOrder,
  type HomeGroup,
  type HomeOrder,
  type HomeRow,
} from "./homeRows";
import { HarnessGlyph } from "../spaces/SpacesPanel";
import { TaskGroups, useTaskLedger } from "../tasks/TaskList";
import { SpacesDrawer } from "../spaces/SpacesMobile";
import { ContextRing } from "./ContextRing";
import { useHomeFreshness } from "../home/freshness";
import { HubBanner } from "../home/HubBanner";
import { MainAskDock, MainCard } from "../home/MainCard";
import { NoMatches, NoSessions, SkeletonRows } from "../home/HomeStates";
import { newSessionPath } from "../spaces/store";
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
  // An idle session with a still-running child ranks as awaiting-wake. Children
  // themselves are not home rows. The child's own rank decides, so a child that
  // is starting (or blocked) on a dropped host still counts.
  const childLiveIds = liveChildParents(snapshot.instances);
  const rows = buildHomeRows({
    spaces,
    interactions: pending,
    titleOf: (instanceId) => hubStore.titleOf(instanceId as Id),
    rollupOf: (instanceId) => hubStore.usageRollupOf(instanceId as Id),
    screenOf: (instanceId) => snapshot.screens[instanceId],
    summaryOf: (instanceId) => hubStore.summaryOf(instanceId as Id),
    eventsOf: (instanceId) => snapshot.events[instanceId],
    childLiveOf: (instanceId) => childLiveIds.has(instanceId),
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
  // home.md §8: before the first successful Hub read the ledger is skeleton,
  // never an empty state; while stale the cached rows are greyed. `grace` is
  // deliberately NOT stale — a foregrounded list keeps its rows and actions.
  const freshness = useHomeFreshness();
  const firstSnapshot = freshness.state === "unknown";
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
  // What the list is painting right now: group order, each group's busy/quiet
  // bucket, and which rows sit behind its idle fold. This is the structure a
  // hold freezes (home.md §3.3).
  const searching = query.trim().length > 0;
  const painted = useMemo<PhoneSnapshot>(
    () => ({ groups, fold: foldStateOf(groups, prefs, searching), quietOpen: searching || prefs.quietOpen }),
    [groups, prefs, searching],
  );
  // The committed structure, read when a hold begins: a press must freeze what
  // is on screen, not a render that has not painted yet.
  const paintedRef = useRef(painted);
  useEffect(() => {
    paintedRef.current = painted;
  }, [painted]);

  const { begin, end, held, snapshot, regionRef } = useHeldOrder<PhoneSnapshot>();
  const startHold = useCallback(
    (source: HoldSource) => begin(source, () => paintedRef.current),
    [begin],
  );

  // The spaces drawer is its own sticky source (home.md §3.3): opening it
  // freezes the list and only closing it releases — taps inside the drawer
  // must not.
  useEffect(() => {
    if (drawerOpen) {
      startHold("sheet");
    } else {
      // Closing the drawer releases its source and the focus it restored to
      // the opener; a later Tab re-establishes the focus hold.
      end("sheet");
      end("focus");
    }
  }, [drawerOpen, startHold, end]);

  // While held, the snapshot owns the structure; live data only replaces each
  // row's content by id. A row that vanished keeps its last painted content as
  // a placeholder, and a row that appeared waits for the release.
  const renderedGroups = useMemo<HomeGroup[]>(() => {
    if (!snapshot) return groups;
    const live = new Map(groups.flatMap((group) => group.rows.map((row) => [row.id, row] as const)));
    return snapshot.groups.map((group) => ({
      ...group,
      rows: group.rows.map((row) => live.get(row.id) ?? row),
    }));
  }, [groups, snapshot]);
  // A row painted at hold start that has since left the live data: it keeps its
  // place as a placeholder, with every control inert until release.
  const staleIds = useMemo<ReadonlySet<string>>(() => {
    if (!snapshot) return new Set<string>();
    const live = new Set(groups.flatMap((group) => group.rows.map((row) => row.id)));
    return new Set(
      snapshot.groups.flatMap((group) => group.rows.map((row) => row.id)).filter((id) => !live.has(id)),
    );
  }, [snapshot, groups]);
  const renderedFold = snapshot ? snapshot.fold : painted.fold;
  const renderedQuietOpen = snapshot ? snapshot.quietOpen : painted.quietOpen;

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
                  <span className={ui.segLabel} title="时钟">
                    时钟
                  </span>
                </button>
                <button
                  type="button"
                  className={ui.segItem}
                  data-testid="home-order-list"
                  aria-pressed={order === "list"}
                  onClick={() => setOrder("list")}
                >
                  <span className={ui.segLabel} title="列表">
                    列表
                  </span>
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
          {renderedGroups.length > 0 ? (
            <div
              className={css.groups}
              ref={regionRef}
              data-order-held={held ? "1" : "0"}
              data-freshness={freshness.state}
              // Stale = the cached rows are grey; a foreground grace keeps
              // them at full strength (home.md §8).
              data-dimmed={freshness.state === "stale" ? "1" : undefined}
              // Only the rows freeze the order; the toolbar and search are
              // outside this container.
              onPointerDown={() => startHold("pointer")}
              // Focus only freezes on a ROW control: the fold toggles live in
              // this container too, and clicking one must take effect at once.
              onFocus={(event) => {
                if ((event.target as Element | null)?.closest('[data-testid="home-row"]')) {
                  startHold("focus");
                }
              }}
              // Mirror the start predicate: the focus hold ends as soon as
              // focus moves anywhere that is not a row control — including the
              // fold toggles in this region, which must take effect at once.
              onBlur={(event) => {
                if (!(event.relatedTarget as Element | null)?.closest('[data-testid="home-row"]')) {
                  end("focus");
                }
              }}
            >
              <HomeGroups
                groups={renderedGroups}
                query={query}
                fold={renderedFold}
                quietOpen={renderedQuietOpen}
                resumeStates={resumeStates}
                staleIds={staleIds}
                onResume={(instanceId) => void onResume(instanceId)}
              />
            </div>
          ) : null}
        </>
      </CommitProbe>
      {/* §8 横幅: after 5 s of consecutive refresh failures. Outside the probe
          (its countdown ticks once a second while failing) and flex-ordered
          right under the sticky head, above the task layer. */}
      <HubBanner />
      {/* D-057 Main card + 问 Main (H8a): outside the probe so its own echo
          and tail reads never report commit:HomeList; flex order 0 keeps it
          right under the sticky head, below the banner. Renders nothing
          without a Main. */}
      <MainCard placement="phone" />
      {/* Outside the probe: its 5s ledger poll never reports commit:HomeList.
          CSS order keeps it under the sticky head and above project groups. */}
      <HomeTaskLayer
        projectFilter={projectFilter}
        instances={instances}
        pending={pending}
        spaces={spaces}
        branches={branches}
        query={query}
        onClearSearch={() => setQuery("")}
        newHref={newSessionPath(active)}
        firstSnapshot={firstSnapshot}
        sessionEmpty={renderedGroups.length === 0}
      />
      {/* D-061 ⑨: 问 Main docked at the bottom, above PhoneNav. */}
      <MainAskDock />
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

/** Header dot counts, one glyph per rank (home.md §3.1): ⚠ blocked, ● working,
 *  ◉ done-unseen, ○ awaiting-wake and idle, a pulsing dot for starting, ■ exited.
 *  A zero count is omitted — an unknown number is never shown as 0. */
function GroupDots({ dots }: { dots: HomeGroup["dots"] }) {
  const parts = [
    dots.blocked > 0 ? `⚠${dots.blocked}` : "",
    dots.working > 0 ? `●${dots.working}` : "",
    dots.unseen > 0 ? `◉${dots.unseen}` : "",
    dots.wake + dots.idle > 0 ? `○${dots.wake + dots.idle}` : "",
    dots.starting > 0 ? `◌${dots.starting}` : "",
    dots.exited > 0 ? `■${dots.exited}` : "",
  ].filter(Boolean);
  if (!parts.length) return null;
  return (
    <span className={css.groupDots} data-testid="home-group-dots">
      {parts.join(" ")}
    </span>
  );
}

function HomeRowView({
  row,
  resumeState,
  onResume,
  stale = false,
}: {
  row: HomeRow;
  resumeState: ResumeState | undefined;
  onResume: (instanceId: string) => void;
  /** Painted during a hold but gone from the live data: show it, let nothing fire. */
  stale?: boolean;
}) {
  return (
    <article
      className={css.row}
      data-testid="home-row"
      data-status={row.status}
      data-rank={row.rank}
      data-blocked={row.blocked ? "1" : "0"}
      data-stale={stale ? "1" : undefined}
      aria-disabled={stale || undefined}
    >
      <Link
        className={css.rowMain}
        to={`/s/${row.id}`}
        data-testid="home-row-link"
        aria-disabled={stale || undefined}
        tabIndex={stale ? -1 : undefined}
        onClick={stale ? (event) => event.preventDefault() : undefined}
      >
        <span className={css.dot}>
          <StateDot status={row.status} />
        </span>
        <span className={css.rowText}>
          <span className={css.rowLine1}>
            <span className={css.rowTitle} title={row.title}>
              {row.title}
            </span>
            <HarnessGlyph kind={row.kind} />
            {row.stateWord ? (
              <span className={css.rowState} data-testid="home-row-state">
                {row.stateWord}
              </span>
            ) : null}
            <span className={css.rowTime} title={row.updatedAt}>
              {row.timeLabel}
            </span>
          </span>
          <span
            className={css.rowBody}
            data-testid="home-row-body"
            data-error={row.bodyIsError ? "1" : "0"}
            data-tone={row.bodyTone ?? undefined}
            title={
              row.bodyDetail && row.bodyDetail !== row.body ? `${row.body}\n${row.bodyDetail}` : row.body
            }
          >
            {row.body}
          </span>
        </span>
        <ContextRing pct={row.contextPct} approximate={row.contextPctApproximate} />
      </Link>
      {row.canResume ? (
        <button
          type="button"
          className={`${ui.btn} ${css.resume}`}
          data-testid="home-resume"
          disabled={stale || resumeState?.busy === true}
          onClick={(event) => {
            event.preventDefault();
            onResume(row.id);
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
}

/** Which rows a group hides, and how many idle rows it holds. */
type FoldState = Record<string, { folded: string[]; idleCount: number }>;

/** The painted phone structure a hold freezes (home.md §3.3). */
type PhoneSnapshot = {
  groups: HomeGroup[];
  fold: FoldState;
  quietOpen: boolean;
};

/** The fold state the current prefs and search imply. */
function foldStateOf(
  groups: readonly HomeGroup[],
  prefs: SpacePrefs,
  searching: boolean,
): FoldState {
  const state: FoldState = {};
  for (const group of groups) {
    const idle = group.rows.filter((row) => rankFolds(row.rank)).map((row) => row.id);
    const idleOpen = searching || prefs.idleOpen[group.id] === true;
    // A project whose every row folds still shows those rows once the quiet
    // bucket itself is open — folding them again would hide the project.
    const showIdle = idleOpen || group.quiet;
    state[group.id] = { folded: showIdle ? [] : idle, idleCount: idle.length };
  }
  return state;
}

/**
 * Project groups with the two folds: idle rows inside a busy project, and
 * whole projects that have nothing active. A non-empty search forces both
 * open. Fold state is the device's SpacePrefs, or the state frozen by a hold.
 */
function HomeGroups({
  groups,
  query,
  fold,
  quietOpen,
  resumeStates,
  staleIds,
  onResume,
}: {
  groups: HomeGroup[];
  query: string;
  fold: FoldState;
  quietOpen: boolean;
  resumeStates: Record<string, ResumeState>;
  staleIds: ReadonlySet<string>;
  onResume: (instanceId: string) => void;
}) {
  const searching = query.trim().length > 0;
  const busy = groups.filter((group) => !group.quiet);
  const quiet = groups.filter((group) => group.quiet);

  function renderGroup(group: HomeGroup) {
    const { folded, idleCount } = fold[group.id] ?? { folded: [], idleCount: 0 };
    const hidden = new Set(folded);
    const visible = group.rows.filter((row) => !hidden.has(row.id));
    return (
      <section
        key={group.id}
        className={css.group}
        data-testid="home-group"
        data-quiet={group.quiet ? "1" : "0"}
        data-blocked={group.blockedCount}
      >
        <header className={css.groupHead}>
          <span className={css.groupProject} title={`${group.project} · ${group.hostName}`}>
            {group.project}
          </span>
          {group.branch ? <span className={css.groupBranch}>{group.branch}</span> : null}
          <GroupDots dots={group.dots} />
        </header>
        {visible.map((row) => (
          <HomeRowView
            key={row.id}
            row={row}
            resumeState={resumeStates[row.id]}
            stale={staleIds.has(row.id)}
            onResume={onResume}
          />
        ))}
        {folded.length > 0 ? (
          <button
            type="button"
            className={css.fold}
            data-testid="home-idle-fold"
            aria-expanded={false}
            onClick={() => spaceStore.toggleIdle(group.id)}
          >
            还有 {folded.length} 个空闲 ▾
          </button>
        ) : null}
        {!searching && folded.length === 0 && !group.quiet && idleCount > 0 ? (
          <button
            type="button"
            className={css.fold}
            data-testid="home-idle-fold"
            aria-expanded
            onClick={() => spaceStore.toggleIdle(group.id)}
          >
            收起空闲 ▴
          </button>
        ) : null}
      </section>
    );
  }

  return (
    <>
      {busy.map(renderGroup)}
      {quiet.length > 0 ? (
        <div className={css.quiet} data-testid="home-quiet">
          {/* Search forces every fold open, so the toggle is not offered: it
              could not collapse anything and would only flip the saved
              preference for after the search. */}
          {searching ? null : (
            <button
              type="button"
              className={css.fold}
              data-testid="home-quiet-fold"
              aria-expanded={quietOpen}
              onClick={() => spaceStore.toggleQuiet()}
            >
              安静的项目 · {quiet.length} {quietOpen ? "▴" : "▸"}
            </button>
          )}
          {quietOpen ? quiet.map(renderGroup) : null}
        </div>
      ) : null}
    </>
  );
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
  onClearSearch,
  newHref,
  firstSnapshot = false,
  sessionEmpty = false,
}: {
  projectFilter: string | null;
  instances: Instance[];
  pending: Interaction[];
  spaces: Space[];
  branches: Record<string, string>;
  query: string;
  /** Clears the search field from the no-match empty state. */
  onClearSearch: () => void;
  /** Where 「新建会话」 goes from the empty state. */
  newHref: string;
  /** No Hub snapshot yet: the ledger already paints skeletons, never an
      empty state (home.md §8). */
  firstSnapshot?: boolean;
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
    // Before the first snapshot there is nothing to say and nothing to
    // create from: paint the three skeleton rows §8 asks for, never
    // 「还没有会话」 (a cold start must not flash an empty state).
    if (firstSnapshot) return <SkeletonRows variant="phone" />;
    return query.trim() ? (
      <NoMatches onClear={onClearSearch} />
    ) : (
      <NoSessions newHref={newHref} />
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
