import { useEffect, useMemo, useRef, useState, type FormEvent, type MouseEvent } from "react";
import { Link, useLocation, useSearchParams } from "react-router-dom";
import type { Id } from "../../types/wire";
import type { Instance, Kind, UiStatus } from "../../types/instance";
import { knowledgeValue } from "../../types/command";
import { Sheet } from "../../components/Sheet";
import { StateDot } from "../../components/StateDot";
import { formatListTime, shortId } from "../../lib/format";
import { nativeShort, projectStatus, uiMode } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import { useWorkbenchViewport } from "../../lib/viewport";
import { modifierAriaShortcut, modifierBadgeText, platformInfo } from "../../lib/platform";
import { useModifierHeld } from "../../lib/useModifierHeld";
import { buildSpaces, useSpacesPrefs } from "../spaces/store";
import { switchSlots } from "../../lib/sessionSlots";
import { isEmberEffort } from "./effort";
import { LaunchedByMark } from "./LaunchedBy";
import {
  applyFilters,
  availableConditions,
  clearedConditions,
  conditionCount,
  describeScope,
  emptyState,
  hasConditions,
  KINDS,
  pruneForScope,
  readConditions,
  selectedChips,
  STATUS_LABELS,
  STATUSES,
  toggleValue,
  withoutChip,
  writeConditions,
  type FilterConditions,
  type SelectedChip,
} from "./sessionFilters";
import css from "./SessionList.module.css";

const GROUPS: { id: string; title: string; match: (s: UiStatus) => boolean }[] = [
  { id: "blocked", title: "待处理", match: (s) => s === "blocked" },
  { id: "working", title: "进行中", match: (s) => s === "working" || s === "starting" },
  { id: "recent", title: "最近", match: (s) => s === "idle" || s === "unknown" },
  // D-028 §8 方案 A: node-epoch-changed settles the row as exited; it leaves
  // the live groups and lands here, next to its Resume button.
  { id: "exited", title: "已退出", match: (s) => s === "exited" },
];

function exitLabel(instance: Instance): string | null {
  if (instance.exit.state !== "known") return null;
  const code = instance.exit.value.code;
  return code == null ? "exit" : `exit ${code}`;
}

function kindClass(kind: Kind): string {
  if (kind === "codex") return css.kindCodex;
  if (kind === "grok") return css.kindGrok;
  if (kind === "agy") return css.kindAgy;
  if (kind === "terminal") return css.kindClaude;
  return css.kindClaude;
}

function stopRow(event: MouseEvent | FormEvent) {
  event.preventDefault();
  event.stopPropagation();
}

function pendingBadge(kind: string | undefined, title: string | undefined, fields: number | undefined): string | null {
  if (kind === "approval") return `等你批准 ${title ?? ""}`.trim();
  if (kind === "question") return `AskUserQuestion · ${fields ?? 0} 题`;
  if (kind === "plan-review") return "计划待审";
  return null;
}

export function SessionList({
  instances,
  variant = "full",
  title = "会话",
  newHref = "/sessions/new",
  space,
}: {
  instances?: Instance[];
  variant?: "full" | "compact";
  title?: string;
  newHref?: string;
  /** The Space the list is pinned to. Absent = the caller is already global. */
  space?: { id: string; name?: string; hostId?: string; workspaceId?: string };
}) {
  const hub = useHub();
  const location = useLocation();
  const { mobile } = useWorkbenchViewport();
  const [params, setParams] = useSearchParams();
  const conditions = readConditions(params);
  const global = conditions.scope === "all";
  const scope = describeScope(conditions, space, (hostId) => hubStore.hostName(hostId as Id));
  const allowed = availableConditions(scope);

  // In global scope the list leaves the Space behind and searches every
  // top-level instance the hub knows about; otherwise it renders exactly what
  // the caller handed it.
  const source = global ? hub.instances.filter((i) => i.parent == null) : (instances ?? hub.instances.filter((i) => i.parent == null));

  const filtered = useMemo(
    () => applyFilters(source, conditions, projectStatus, { titleOf: (id) => hubStore.titleOf(id as Id), workspaces: hub.workspaces }),
    // `conditions` is rebuilt each render from params; key the memo on the
    // serialized params instead so it only recomputes when the URL changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [source, params.toString(), hub.workspaces],
  );

  const filterCount = conditionCount(conditions);
  const live = hub.connection === "live";

  // The one shared ordering behind ⌘/Ctrl+1–9: Shell's keydown handler resolves
  // digits through the same switchSlots() call, so a badge can never name a
  // session the keypress would not open (filters and status groups only change
  // where a slotted row is painted, never its number).
  const prefs = useSpacesPrefs();
  const slots = useMemo(() => {
    if (!space?.id) return [] as ReturnType<typeof switchSlots>;
    const fullSpace = buildSpaces(hub.workspaces, hub.instances, prefs).find((item) => item.id === space.id);
    return switchSlots(fullSpace, prefs);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [space?.id, hub.workspaces, hub.instances, prefs]);
  const slotById = useMemo(() => new Map(slots.map((instance, index) => [instance.id, index + 1])), [slots]);
  const platform = platformInfo();
  // No hold gesture on a phone or a touch platform; the hook never attaches a
  // listener there and neither the badge nor the shortcut claim is rendered.
  const gestureEnabled = !mobile && platform.heldModifiersSupported;
  const modifierHeld = useModifierHeld(gestureEnabled);

  const [selected, setSelected] = useState<string[]>([]);
  const [broadcast, setBroadcast] = useState("");
  const [filterOpen, setFilterOpen] = useState(false);
  const filterBtnRef = useRef<HTMLButtonElement | null>(null);
  const filterHeadingId = "session-filter-title";

  /**
   * Explicit condition changes push a history entry, so Back undoes exactly the
   * filter the user chose. Typing in the search box replaces instead — a
   * keystroke is not a decision worth its own entry (P0-1 rule 3).
   *
   * The update is a function of the *current* params rather than the ones this
   * render closed over: two changes in quick succession (clicking a scope and
   * immediately typing) must compose, not clobber each other.
   */
  const commit = (update: (current: FilterConditions) => FilterConditions, mode: "push" | "replace") =>
    setParams((prev) => writeConditions(prev, update(readConditions(prev))), { replace: mode === "replace" });

  const ptyKey = source
    .filter((instance) => uiMode(instance) === "tty-attachable")
    .map((instance) => instance.id)
    .join(",");

  // Switching Space drops host/workspace conditions that the new fixed scope
  // cannot honour, in exactly one replace, keeping text and status (§4 risk 2).
  // Derivation runs one way: params are rewritten here, never the Space store.
  const spaceId = space?.id;
  const pruned = pruneForScope(conditions, scope);
  const prunedKey = pruned.dropped.map((chip) => `${chip.key}:${chip.value}`).join(",");
  // Keyed by Space so the message survives the rewrite that clears `prunedKey`
  // and disappears on the next Space, rather than a render later.
  const [droppedNotice, setDroppedNotice] = useState<{ spaceId?: string; chips: SelectedChip[] }>({ chips: [] });
  useEffect(() => {
    if (!prunedKey) return;
    setDroppedNotice({ spaceId, chips: pruned.dropped });
    // Re-read the params inside the updater: this effect runs after a commit
    // that may itself have changed the URL (switching to global scope is one),
    // and writing back the render's stale copy would undo it.
    setParams((prev) => {
      const current = readConditions(prev);
      const verdict = pruneForScope(current, describeScope(current, space, (hostId) => hubStore.hostName(hostId as Id)));
      return verdict.dropped.length ? writeConditions(prev, verdict.conditions) : prev;
    }, { replace: true });
    // Re-run only when the pruning verdict itself changes, not on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [spaceId, prunedKey]);
  // The notice belongs to the Space that triggered it; a later Space retires it.
  const notice = droppedNotice.spaceId === spaceId ? droppedNotice.chips : [];

  useEffect(() => {
    if (variant !== "full" || !ptyKey) return;
    const ids = ptyKey.split(",") as Id[];
    void hubStore.refreshScreens(ids);
    const timer = window.setInterval(() => void hubStore.refreshScreens(ids), 2500);
    return () => window.clearInterval(timer);
  }, [variant, ptyKey]);

  const chips = selectedChips(conditions, {
    hostName: (id) => hubStore.hostName(id as Id),
    workspaceLabel: (id) => hub.workspaces.find((w) => w.id === id)?.label ?? shortId(id, 8),
    workspaceAmbiguous: (id) => {
      const label = hub.workspaces.find((w) => w.id === id)?.label;
      return Boolean(label) && hub.workspaces.filter((w) => w.label === label).length > 1;
    },
    workspaceHost: (id) => {
      const hostId = hub.workspaces.find((w) => w.id === id)?.hostId;
      return hostId ? hubStore.hostName(hostId) : "—";
    },
  });

  const empty = emptyState({
    hostCount: hub.hosts.length,
    workspaceCount: hub.workspaces.length,
    sourceCount: source.length,
    matchCount: filtered.length,
    conditions,
  });

  if (empty === "no-hosts") {
    return (
      <p className={css.empty} data-testid="session-list" data-empty="no-hosts">
        无主机。<Link to="/hosts">添加主机</Link>
      </p>
    );
  }
  if (empty === "no-workspaces") {
    return (
      <p className={css.empty} data-testid="session-list" data-empty="no-workspaces">
        主机已连接，但还没有注册工作目录。<Link to="/hosts">注册工作目录</Link>
      </p>
    );
  }
  if (empty === "no-sessions") {
    return (
      <p className={css.empty} data-testid="session-list" data-empty="no-sessions">
        {title} 还没有会话。<Link to={newHref}>新建会话</Link>
      </p>
    );
  }

  if (variant === "compact") {
    return (
      <div className={css.root} data-testid="session-list">
        <header className={css.compactTop}>
          <div className={css.compactTitle}>会话</div>
          {conditions.host.length ? <div className={css.compactHint}>host:{conditions.host.length}</div> : null}
        </header>
        {filtered.map((instance) => {
          const status = projectStatus(instance);
          const workspace = hub.workspaces.find((w) => w.id === instance.workspaceId && w.hostId === instance.hostId);
          const to = `/s/${instance.id}`;
          const active = location.pathname === to || location.pathname.startsWith(`${to}/`);
          return (
            <Link
              key={instance.id}
              to={to}
              className={`${css.compactRow} ${active ? css.compactRowActive : ""}`}
              data-testid="session-row"
              data-status={status}
            >
              <div className={css.compactHead}>
                <StateDot status={status} />
                <span className={`${css.compactName} ${status === "exited" || status === "unknown" ? css.compactNameMute : ""}`}>
                  {hubStore.titleOf(instance.id)}
                </span>
              </div>
              <div className={css.compactMeta}>
                {hubStore.hostName(instance.hostId)} / {workspace?.label} · {formatListTime(instance.updatedAt)}
              </div>
            </Link>
          );
        })}
      </div>
    );
  }

  return (
    <div className={css.root} data-testid="session-list">
      <header className={css.top}>
        <div className={css.title}>{title}</div>
        <div className={css.count}>
          {source.length} 个实例 · {new Set(source.map((i) => i.hostId)).size} 台主机
        </div>
        <div className={css.live}>
          <span className={`${css.liveDot} ${live ? "" : css.liveOff}`} />
          {live ? "live" : hub.connection}
        </div>
        <button
          type="button"
          className={css.filterBtn}
          data-testid="session-filter-open"
          ref={filterBtnRef}
          aria-expanded={filterOpen}
          aria-haspopup="dialog"
          aria-controls={filterOpen ? "session-filter-panel" : undefined}
          onClick={() => setFilterOpen((open) => !open)}
        >
          筛选{filterCount ? <span className={css.filterCount}>{filterCount}</span> : null}
        </button>
        <Link className={css.newBtn} to={newHref}>
          ＋ 新建
        </Link>
      </header>
      <div className={css.toolbar}>
        <div className={css.toolbarTop}>
          <label className={css.search}>
            <span className={css.searchGlyph}>⌕</span>
            <input
              className={css.searchInput}
              data-testid="session-search"
              aria-label="搜索会话"
              placeholder="搜索标题 / cwd / 原生 id"
              value={conditions.q}
              // Typing replaces the history entry: Back should undo the filter
              // the user chose, not each keystroke on the way there.
              onChange={(e) => commit((current) => ({ ...current, q: e.target.value }), "replace")}
            />
          </label>
          <div className={css.scope} data-testid="session-scope" data-scope={scope.kind}>
            {scope.label}
          </div>
          <div className={css.matchCount} data-testid="session-match-count">
            {filtered.length} / {source.length} 个会话
          </div>
          {global ? (
            <button
              type="button"
              className={css.scopeBtn}
              data-testid="session-scope-space"
              onClick={() => commit((current) => ({ ...current, scope: "space" }), "push")}
              disabled={!space?.hostId}
            >
              回到当前 Space
            </button>
          ) : (
            <button
              type="button"
              className={css.scopeBtn}
              data-testid="session-scope-all"
              onClick={() => commit((current) => ({ ...current, scope: "all" }), "push")}
            >
              搜索所有空间
            </button>
          )}
        </div>
        {notice.length ? (
          <div className={css.notice} data-testid="session-scope-notice" role="status">
            切换 Space 已清除不适用的条件（{notice.length} 项主机 / 目录），保留了文本与状态条件。
          </div>
        ) : null}
        {chips.length ? (
          <div className={css.chips} data-testid="session-selected-chips">
            {chips.map((chip) => (
              <button
                key={`${chip.key}:${chip.value}`}
                type="button"
                className={`${css.chip} ${css.chipOn}`}
                data-testid="session-chip"
                data-chip-key={chip.key}
                aria-label={`移除条件 ${chip.label}`}
                onClick={() => commit((current) => withoutChip(current, chip), "push")}
              >
                {chip.label}
                <span className={css.chipX} aria-hidden="true">
                  ✕
                </span>
              </button>
            ))}
            <button
              type="button"
              className={css.clearAll}
              data-testid="session-clear-filters"
              onClick={() => commit(clearedConditions, "push")}
            >
              清除筛选
            </button>
          </div>
        ) : null}
        <Sheet
          open={filterOpen}
          onClose={() => setFilterOpen(false)}
          variant={mobile ? "sheet" : "popover"}
          labelledBy={filterHeadingId}
          returnFocusRef={filterBtnRef}
          testId="session-filter-panel"
        >
          <div className={css.panelHead}>
            <h2 className={css.panelTitle} id={filterHeadingId}>
              筛选会话
            </h2>
            <button type="button" className={css.panelClose} data-testid="session-filter-close" onClick={() => setFilterOpen(false)}>
              关闭
            </button>
          </div>
          <p className={css.panelScope}>{scope.label}</p>
          <fieldset className={css.panelGroup}>
            <legend className={css.panelLegend}>状态</legend>
            {STATUSES.map((s) => (
              <button
                key={s}
                type="button"
                className={`${css.chip} ${conditions.status.includes(s) ? css.chipOn : ""}`}
                data-testid={`session-filter-status-${s}`}
                aria-pressed={conditions.status.includes(s)}
                onClick={() => commit((current) => ({ ...current, status: toggleValue(current.status, s) }), "push")}
              >
                {STATUS_LABELS[s] ?? s}
              </button>
            ))}
          </fieldset>
          <fieldset className={css.panelGroup}>
            <legend className={css.panelLegend}>类型</legend>
            {KINDS.map((k) => (
              <button
                key={k}
                type="button"
                className={`${css.chip} ${conditions.kind.includes(k) ? css.chipOn : ""}`}
                data-testid={`session-filter-kind-${k}`}
                aria-pressed={conditions.kind.includes(k)}
                onClick={() => commit((current) => ({ ...current, kind: toggleValue(current.kind, k) }), "push")}
              >
                {k}
              </button>
            ))}
          </fieldset>
          {/* A fixed Space already pins host and workspace; offering them here
              could only build a self-excluding query (P0-1 rule 2). */}
          {allowed.host ? (
            <fieldset className={css.panelGroup} data-testid="session-filter-hosts">
              <legend className={css.panelLegend}>主机</legend>
              {hub.hosts.map((h) => (
                <button
                  key={h.id}
                  type="button"
                  className={`${css.chip} ${conditions.host.includes(h.id) ? css.chipOn : ""}`}
                  aria-pressed={conditions.host.includes(h.id)}
                  onClick={() => commit((current) => ({ ...current, host: toggleValue(current.host, h.id) }), "push")}
                >
                  {h.label}
                </button>
              ))}
            </fieldset>
          ) : null}
          {allowed.workspace ? (
            <fieldset className={css.panelGroup} data-testid="session-filter-workspaces">
              <legend className={css.panelLegend}>工作目录</legend>
              {hub.workspaces.map((w) => {
                const ambiguous = hub.workspaces.filter((other) => other.label === w.label).length > 1;
                return (
                  <button
                    key={w.id}
                    type="button"
                    className={`${css.chip} ${conditions.workspace.includes(w.id) ? css.chipOn : ""}`}
                    aria-pressed={conditions.workspace.includes(w.id)}
                    onClick={() => commit((current) => ({ ...current, workspace: toggleValue(current.workspace, w.id) }), "push")}
                  >
                    {/* Same-name directories on different hosts are only
                        distinguishable once the host is spelled out (§2.2). */}
                    {ambiguous ? `${w.label} · ${hubStore.hostName(w.hostId)}` : w.label}
                  </button>
                );
              })}
            </fieldset>
          ) : (
            <p className={css.panelNote} data-testid="session-filter-scope-note">
              当前 Space 已固定主机与工作目录。要按主机或目录筛选，请先“搜索所有空间”。
            </p>
          )}
          <div className={css.panelFoot}>
            <button
              type="button"
              className={css.clearAll}
              data-testid="session-filter-clear"
              disabled={!hasConditions(conditions)}
              onClick={() => commit(clearedConditions, "push")}
            >
              清除筛选
            </button>
          </div>
        </Sheet>
        {selected.length ? (
          <form
            className={css.fleet}
            data-testid="board-fleet"
            onSubmit={(event) => {
              event.preventDefault();
              void hubStore.broadcast(selected as Id[], broadcast).then(() => setBroadcast(""));
            }}
          >
            <span className={css.fleetCount}>{selected.length} 已选</span>
            <input
              className={css.fleetInput}
              data-testid="board-broadcast"
              placeholder="群发文本…"
              value={broadcast}
              onChange={(event) => setBroadcast(event.target.value)}
            />
            <button type="submit" className={css.fleetSend} data-testid="board-broadcast-send" disabled={!broadcast.trim()}>
              群发
            </button>
          </form>
        ) : null}
      </div>
      {empty === "no-matches" ? (
        <div className={css.noMatches} data-testid="session-no-matches" data-empty="no-matches">
          <p className={css.noMatchesTitle}>没有会话符合当前筛选条件。</p>
          <p className={css.noMatchesBody}>
            {source.length} 个会话在{scope.kind === "space" ? "当前 Space" : "所有空间"}内，但都不匹配。清除条件只改变列表显示，不会创建或关闭任何会话。
          </p>
          <div className={css.noMatchesActions}>
            <button
              type="button"
              className={css.noMatchesBtn}
              data-testid="session-no-matches-clear"
              onClick={() => commit(clearedConditions, "push")}
            >
              清除筛选
            </button>
            {!global ? (
              <button
                type="button"
                className={css.noMatchesBtn}
                data-testid="session-no-matches-all"
                onClick={() => commit((current) => ({ ...current, scope: "all" }), "push")}
              >
                搜索所有空间
              </button>
            ) : null}
          </div>
        </div>
      ) : null}
      {GROUPS.map((group) => {
        const items = filtered.filter((i) => group.match(projectStatus(i)));
        if (!items.length) return null;
        return (
          <section key={group.id} data-testid={`session-group-${group.id}`}>
            <div className={css.group}>
              <div className={`${css.groupTitle} ${group.id === "blocked" ? css.groupTitleBlocked : ""}`}>{group.title}</div>
              <div className={css.groupMeta}>
                {items.length}
                {group.id === "blocked" ? " · 置顶，不进折叠组" : group.id === "recent" ? " · 子 agent 不在此列" : ""}
              </div>
            </div>
            {items.map((instance) => {
              const status = projectStatus(instance);
              const pending = hub.interactions.find((i) => i.instanceId === instance.id && i.state === "pending");
              const to = `/s/${instance.id}`;
              const workspace = hub.workspaces.find((w) => w.id === instance.workspaceId && w.hostId === instance.hostId);
              const badge = pendingBadge(
                pending?.request.kind,
                pending && pending.request.kind !== "elicitation" ? pending.request.title : undefined,
                pending?.request.kind === "question" ? pending.request.fields.length : undefined,
              );
              const summary = hubStore.summaryOf(instance.id);
              const tty = uiMode(instance) === "tty-attachable";
              const compactRecent = status === "idle" || status === "exited" || status === "unknown";
              const title = hubStore.titleOf(instance.id);
              const activity = knowledgeValue(instance.activity) ?? "—";
              const screen = hub.screens[instance.id];
              const checked = selected.includes(instance.id);
              const worktree = workspace?.worktreeLabel ?? workspace?.label;
              const branch = workspace?.branch;
              const slot = slotById.get(instance.id) ?? 0;
              return (
                <article
                  key={instance.id}
                  className={`${css.row} ${status === "blocked" ? css.rowBlocked : ""} ${compactRecent ? css.rowIdle : ""}`}
                  data-testid="board-card"
                  data-status={status}
                  data-lifecycle={instance.lifecycle}
                  data-kind={instance.kind}
                >
                  <label className={css.check}>
                    <input
                      type="checkbox"
                      data-testid="board-select"
                      checked={checked}
                      onChange={() => {
                        setSelected((cur) => (cur.includes(instance.id) ? cur.filter((id) => id !== instance.id) : [...cur, instance.id]));
                      }}
                    />
                  </label>
                  <Link
                    to={to}
                    className={`${css.body} ${compactRecent ? css.bodyIdle : ""}`}
                    data-testid="session-row"
                    data-status={status}
                    data-kind={instance.kind}
                    aria-keyshortcuts={gestureEnabled && slot ? modifierAriaShortcut(slot, platform) : undefined}
                  >
                    <div className={css.meta} data-testid="session-lifecycle">
                      <span>{instance.lifecycle}</span>
                      <span className={css.sep}>·</span>
                      <span>{activity}</span>
                      <span className={css.sep}>·</span>
                      <span>{instance.connectivity}</span>
                      <span className={css.sep}>|</span>
                      <span className={css.metaHost}>{hubStore.hostName(instance.hostId)}</span>
                      <span>/ {worktree}</span>
                      {branch ? <span className={css.branch}>{branch}</span> : null}
                      <span>· {instance.driver}</span>
                      {(() => {
                        const effort = hubStore.effortOf(instance.id, instance.kind);
                        // §9.1: the list row shows the transcript-read-back
                        // effective level too, with `?` until it is observed.
                        const effective = hubStore.effortEffectiveOf(instance.id);
                        const effectiveName = effective?.name ?? "?";
                        const mismatch =
                          effective &&
                          (effort.ultracode === true
                            ? !(effective.name === "xhigh" && effective.ultracode === true)
                            : effort.name !== effective.name);
                        const ember = isEmberEffort(instance.kind, effort.index, effort.ultracode);
                        return (
                          <>
                            <span className={css.sep}>·</span>
                            <span
                              className={ember ? css.effortEmber : undefined}
                              data-testid="session-effort"
                              data-ember={ember ? "1" : "0"}
                              data-effort-effective={effective ? effective.name : "unknown"}
                              data-effort-mismatch={mismatch ? "1" : "0"}
                              title={
                                effective
                                  ? `请求 ${effort.ultracode ? "ultracode" : effort.name} · 实际 ${effective.name}（${effective.source}）`
                                  : "实际档位尚未从会话回读"
                              }
                            >
                              {effectiveName}
                            </span>
                          </>
                        );
                      })()}
                      <span className={css.sep}>|</span>
                      <span>{shortId(instance.id, 8)}</span>
                    </div>
                    <div className={css.headline}>
                      <StateDot status={status} />
                      <div className={`${css.name} ${status === "starting" || status === "exited" || status === "unknown" ? css.nameMute : ""}`}>
                        {title}
                      </div>
                      <span className={`${css.kind} ${kindClass(instance.kind)}`}>{instance.kind}</span>
                      <LaunchedByMark launchedBy={instance.launchedBy} />
                      {screen?.done ? (
                        <span className={css.done} data-testid="board-done">
                          DONE
                        </span>
                      ) : null}
                      {badge ? <div className={css.badge}>{badge}</div> : null}
                      {exitLabel(instance) ? <div className={css.exit}>{exitLabel(instance)}</div> : null}
                      {gestureEnabled && slot ? (
                        <span
                          className={css.keyBadge}
                          data-held={modifierHeld ? "1" : "0"}
                          aria-hidden="true"
                        >
                          {modifierBadgeText(slot, platform)}
                        </span>
                      ) : null}
                    </div>
                    {tty && screen?.lines.length ? (
                      <pre className={css.snippet} data-testid="board-snippet">
                        {screen.lines.join("\n")}
                      </pre>
                    ) : status === "starting" ? (
                      <div className={css.cmd}>正在拉起 · lifecycle={instance.lifecycle}</div>
                    ) : status === "blocked" && pending?.request.kind === "approval" ? (
                      <div className={css.cmd}>{pending.request.description}</div>
                    ) : summary && status !== "blocked" ? (
                      <div className={css.cmd}>{summary}</div>
                    ) : status === "idle" ? (
                      <div className={css.cmd}>回合结束、进程仍在 · 可继续 send</div>
                    ) : status === "unknown" ? (
                      <div className={css.cmd}>connectivity={instance.connectivity} · 不推断成功或结束</div>
                    ) : null}
                  </Link>
                  <form
                    className={css.actions}
                    onSubmit={(event) => {
                      stopRow(event);
                      const form = event.currentTarget;
                      const input = form.elements.namedItem("prompt") as HTMLInputElement | null;
                      const text = input?.value.trim() ?? "";
                      if (text) {
                        void hubStore.send(instance.id, text);
                        if (input) input.value = "";
                      }
                    }}
                  >
                    <input
                      className={css.actionInput}
                      name="prompt"
                      data-testid="board-prompt"
                      placeholder="send…"
                      onClick={(event) => event.stopPropagation()}
                    />
                    <button type="submit" className={css.actionBtn} data-testid="board-send">
                      发送
                    </button>
                    <button
                      type="button"
                      className={css.actionBtn}
                      data-testid="board-key-enter"
                      onClick={(event) => {
                        event.stopPropagation();
                        void hubStore.sendKeys(instance.id, "enter");
                      }}
                    >
                      enter
                    </button>
                    <button
                      type="button"
                      className={css.actionBtn}
                      data-testid="board-key-esc"
                      onClick={(event) => {
                        event.stopPropagation();
                        void hubStore.sendKeys(instance.id, "esc");
                      }}
                    >
                      esc
                    </button>
                    <button
                      type="button"
                      className={css.actionBtn}
                      data-testid="board-key-ctrl-c"
                      onClick={(event) => {
                        event.stopPropagation();
                        void hubStore.sendKeys(instance.id, "ctrl+c");
                      }}
                    >
                      ctrl+c
                    </button>
                    <button
                      type="button"
                      className={css.actionBtn}
                      data-testid="board-stop"
                      onClick={(event) => {
                        event.stopPropagation();
                        void hubStore.close(instance.id);
                      }}
                    >
                      stop
                    </button>
                    {tty ? (
                      <span className={css.tty} title={nativeShort(instance)}>
                        终端
                      </span>
                    ) : null}
                    <span className={css.time}>{status === "unknown" ? "—" : formatListTime(instance.updatedAt)}</span>
                  </form>
                </article>
              );
            })}
          </section>
        );
      })}
      {gestureEnabled && slots.length ? (
        <footer className={css.switchHint} data-testid="session-switch-hint" aria-hidden="true">
          按住 {platform.glyph} 快捷切换
        </footer>
      ) : null}
    </div>
  );
}
