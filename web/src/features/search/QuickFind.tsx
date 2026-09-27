import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { useNavigate } from "react-router-dom";
import { Sheet } from "../../components/Sheet";
import { StateDot } from "../../components/StateDot";
import { fetchChanges } from "../files/filesApi";
import { isTypingTarget } from "../../lib/keyboardScope";
import { hubStore, useHub } from "../../lib/store";
import { useWorkbenchViewport } from "../../lib/viewport";
import { HarnessGlyph } from "../spaces/SpacesPanel";
import { buildSpaces, spaceKey, spaceStore, useSpacesPrefs } from "../spaces/store";
import {
  groupQuickFind,
  rankQuickFind,
  readQuickFindOrder,
  writeQuickFindOrder,
  type QuickFindGroup,
  type QuickFindHit,
  type QuickFindOrder,
} from "./quickFindSearch";
import css from "./quickfind.module.css";

/**
 * Cross-Space QuickFind (exploration §5 P1-1).
 *
 * The finder searches only the instance/space/host metadata the Hub has
 * already loaded — no remote index, never message bodies. Opening it never
 * writes the URL or the active Space: Escape returns exactly where the user
 * was, and Enter is the only thing that navigates.
 *
 * Desktop ⌘K keeps the flat ranked listbox; the phone Jump To sheet (opened
 * grouped from the terminal key bar) renders the same hits grouped by
 * project + branch with the blocked count and a clock/list toggle. Both
 * shapes are one panel over one ranked list — never a second space model
 * (ui-spec §4.7 / §1.3: a group's leaves are sessions, no pane hierarchy).
 */

/** The agreed shortcut (exploration §5 P1-1, UI spec §2.3): ⌘K / Ctrl+K. */
export const QUICKFIND_KEY = "k";
export const QUICKFIND_HINT = "⌘/Ctrl+K";

/*
 * Tiny external open-state store. The trigger lives in SpacesPanel's header
 * while the global shortcut lives in the always-mounted overlay; an event or
 * prop chain would make the panel re-render on every keystroke, and batch C
 * owns Shell.tsx where an app-wide provider would otherwise mount.
 *
 * `grouped` is the *requested* presentation: only the phone key bar asks for
 * it, and the component still ANDs it with its compact viewport, so desktop
 * ⌘K and every desktop trigger keep the flat listbox regardless.
 */
let openState = false;
let groupedRequest = false;
const openListeners = new Set<() => void>();
function emitOpen() {
  for (const listener of openListeners) listener();
}
export function openQuickFind(options?: { grouped?: boolean }) {
  if (openState) return;
  groupedRequest = options?.grouped ?? false;
  openState = true;
  emitOpen();
}
export function closeQuickFind() {
  if (!openState) return;
  openState = false;
  // The desktop ⌘K default must not inherit a phone session's grouped mode.
  groupedRequest = false;
  emitOpen();
}
function subscribe(listener: () => void) {
  openListeners.add(listener);
  return () => {
    openListeners.delete(listener);
  };
}

/**
 * The global shortcut, safe next to an attached terminal (UX plan §4 risk 1):
 * it never fires while focus is in an input/textarea/select/contenteditable
 * or inside `.xterm`, where the native process owns every key — including
 * ⌘K, which a TUI may bind itself.
 */
function isQuickFindShortcut(event: KeyboardEvent): boolean {
  if (!(event.metaKey || event.ctrlKey) || event.altKey || event.isComposing || event.repeat) return false;
  return event.key.toLowerCase() === QUICKFIND_KEY;
}

function formatUpdated(iso: string): string {
  const time = Date.parse(iso);
  if (!time) return "";
  return new Date(time).toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
}

const STATUS_TEXT: Record<string, string> = {
  blocked: "待处理",
  working: "进行中",
  starting: "启动中",
  idle: "空闲",
  exited: "已退出",
  unknown: "状态待确认",
};

function ResultRow({ hit, selected, onSelect, index }: {
  hit: QuickFindHit;
  selected: boolean;
  onSelect: () => void;
  index: number;
}) {
  return (
    <div
      id={`quickfind-option-${index}`}
      className={css.option}
      role="option"
      aria-selected={selected}
      data-selected={selected}
      data-testid="quickfind-result"
      data-instance-id={hit.instance.id}
      onMouseDown={(event) => {
        // onMouseDown (not onClick) so selection activates before the scrim's
        // mousedown-close logic and focus never has to leave the input.
        event.preventDefault();
        onSelect();
      }}
    >
      <HarnessGlyph kind={hit.instance.kind} />
      <span className={css.optionBody}>
        <span className={css.optionTitle}>{hit.title}</span>
        <span className={css.optionMeta}>
          {/* Space + host are always shown; same-titled rows rely on them. */}
          <span className={css.optionScope}>{hit.spaceName} · {hit.hostName}</span>
          <span className={css.optionStatus}>
            <StateDot status={hit.status} />
            {STATUS_TEXT[hit.status] ?? hit.status}
          </span>
          <span className={css.optionTime}>{formatUpdated(hit.instance.updatedAt)}</span>
        </span>
      </span>
    </div>
  );
}

export function QuickFind({ onNavigate, inline = false }: { onNavigate?: () => void; inline?: boolean }) {
  const hub = useHub();
  const prefs = useSpacesPrefs();
  const { mobile } = useWorkbenchViewport();
  const navigate = useNavigate();
  const open = useSyncExternalStore(subscribe, () => openState);
  const groupedWanted = useSyncExternalStore(subscribe, () => groupedRequest);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  // The phone home owns a sibling key; Jump To gets its own so a desktop
  // user's flat ⌘K list never inherits the phone's last toggle.
  const [order, setOrder] = useState<QuickFindOrder>(() => readQuickFindOrder(localStorageAccess()));
  const [branches, setBranches] = useState<Record<string, string>>({});

  // Grouped presentation exists only on the phone Jump To sheet: the opener
  // asks for it AND the viewport is compact. Desktop ⌘K, the sidebar trigger
  // and a window grown mid-session all keep the flat ranked listbox.
  const grouped = open && groupedWanted && mobile;

  const spaces = useMemo(() => buildSpaces(hub.workspaces, hub.instances, prefs), [hub.workspaces, hub.instances, prefs]);
  const result = useMemo(
    () =>
      rankQuickFind({
        spaces,
        query,
        titleOf: (id) => hubStore.titleOf(id),
        hostNameOf: (id) => (id ? hubStore.hostName(id) : ""),
        connection: hub.connection,
      }),
    [spaces, query, hub],
  );
  const hits = result.hits;

  // The registry's branch is absent on minimal Node replies; the live SCM
  // branch fetched below overrides it per (host, workspace) key.
  const workspacesWithBranch = useMemo(
    () =>
      hub.workspaces.map((workspace) => {
        const live = branches[spaceKey(workspace.hostId, workspace.id)];
        return live ? { ...workspace, branch: live } : workspace;
      }),
    [hub.workspaces, branches],
  );

  const groups = useMemo(
    () => (grouped ? groupQuickFind(hits, spaces, workspacesWithBranch, order) : []),
    [grouped, hits, spaces, workspacesWithBranch, order],
  );

  // In grouped mode the DOM reads group-by-group — groups sorted by the
  // chosen order, blocked leaves pinned inside each group — which differs
  // from rankQuickFind's flat recency array. Cursor wrap, active index,
  // option ids, aria-activedescendant and scroll must follow the VISUAL
  // reading order, so flatten the groups in render order. Flat mode is the
  // ranked array itself, so desktop behaviour is byte-identical.
  const orderedHits = useMemo(
    () => (grouped ? groups.flatMap((group) => group.hits) : hits),
    [grouped, groups, hits],
  );
  // Visual index assigned to each leaf while walking the groups; this is the
  // id/data-index the option carries and therefore what the cursor addresses.
  const sections = useMemo(() => {
    if (!grouped) return [] as { group: QuickFindGroup; leaves: { hit: QuickFindHit; index: number }[] }[];
    let visualIndex = 0;
    return groups.map((group) => ({
      group,
      leaves: group.hits.map((hit) => ({ hit, index: visualIndex++ })),
    }));
  }, [grouped, groups]);

  useEffect(() => {
    writeQuickFindOrder(order, localStorageAccess());
  }, [order]);

  // Live git branch per project, the same read-only changes proxy the phone
  // home uses (HomeList): one fetch per Space id, cached for the component's
  // life; unknown/denied/unreachable simply leaves the branch off the header.
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
    if (!grouped) return;
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
  }, [branchCandidates, grouped]);

  // Reset the query on the open transition. Adjusting state during render is
  // the React-sanctioned form of "reset when a prop/store value changes" — an
  // effect would paint the stale query for one frame first.
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) {
      setQuery("");
      setCursor(0);
    }
  }

  // The query can shrink past the cursor; clamp at render instead of an effect.
  const active = orderedHits.length ? Math.min(cursor, orderedHits.length - 1) : 0;

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!isQuickFindShortcut(event)) return;
      if (isTypingTarget(event.target)) return;
      event.preventDefault();
      openQuickFind();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  function choose(hit: QuickFindHit) {
    // Enter is the one action that changes navigation context. It mirrors the
    // sidebar row: record the Space/tab, then route without a full reload.
    spaceStore.selectTab(hit.spaceId, hit.instance.id);
    closeQuickFind();
    navigate(`/s/${hit.instance.id}`);
    onNavigate?.();
  }

  function onInputKeyDown(event: React.KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (orderedHits.length) setCursor((value) => (value + 1) % orderedHits.length);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (orderedHits.length) setCursor((value) => (value - 1 + orderedHits.length) % orderedHits.length);
    } else if (event.key === "Home" && event.ctrlKey) {
      event.preventDefault();
      setCursor(0);
    } else if (event.key === "End" && event.ctrlKey) {
      event.preventDefault();
      setCursor(Math.max(0, orderedHits.length - 1));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const hit = orderedHits[active];
      if (hit) choose(hit);
    }
  }

  // Keep the active option scrolled into view during arrow-key navigation.
  // The optional call keeps this safe in DOM environments without a layout
  // engine (jsdom), where scrollIntoView does not exist.
  useEffect(() => {
    if (!open) return;
    listRef.current
      ?.querySelector<HTMLElement>(`[data-index="${active}"]`)
      ?.scrollIntoView?.({ block: "nearest" });
  }, [active, open]);

  const headingId = "quickfind-heading";
  const listId = "quickfind-listbox";

  const panel = (
    <Sheet
      open={open}
      onClose={closeQuickFind}
      variant={mobile ? "sheet" : "popover"}
      labelledBy={headingId}
      initialFocusRef={inputRef}
      className={css.sheet}
      testId="quickfind-panel"
    >
      <div className={css.head}>
        <h2 className={css.title} id={headingId}>
          快速查找
        </h2>
        <button type="button" className={css.close} data-testid="quickfind-close" onClick={closeQuickFind}>
          关闭
        </button>
      </div>
      <input
        ref={inputRef}
        className={css.input}
        data-testid="quickfind-input"
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={hits.length ? `quickfind-option-${active}` : undefined}
        placeholder="搜索会话标题、空间、主机…"
        value={query}
        onChange={(event) => {
          setQuery(event.target.value);
          setCursor(0);
        }}
        onKeyDown={onInputKeyDown}
        spellCheck={false}
        autoComplete="off"
      />
      {result.cacheOnly ? (
        <p className={css.cacheNote} data-testid="quickfind-cache-only">
          Hub 离线或重连中：只搜索本机已缓存的会话，可能不是完整列表。
        </p>
      ) : null}
      <div className={css.listHead}>
        <span>{query.trim() ? `${result.total} 个匹配` : `${result.total} 个已缓存会话`}</span>
        {grouped ? (
          <div className={css.order} role="group" aria-label="排序方式">
            <button
              type="button"
              data-testid="quickfind-order-clock"
              aria-pressed={order === "clock"}
              onClick={() => {
                setOrder("clock");
                setCursor(0);
              }}
            >
              时钟
            </button>
            <button
              type="button"
              data-testid="quickfind-order-list"
              aria-pressed={order === "list"}
              onClick={() => {
                setOrder("list");
                setCursor(0);
              }}
            >
              列表
            </button>
          </div>
        ) : (
          <span className={css.scopeNote}>仅已加载的标题 / 空间 / 主机 / ID</span>
        )}
      </div>
      {hits.length ? (
        <div className={css.list} id={listId} role="listbox" aria-label="会话" ref={listRef}>
          {grouped
            ? sections.map(({ group, leaves }) => (
                <div
                  key={group.id}
                  className={css.group}
                  role="group"
                  aria-label={`${group.project} · ${group.hostName}`}
                  data-testid="quickfind-group"
                  data-blocked={group.blockedCount}
                >
                  <div className={css.groupHead} role="presentation">
                    <span
                      className={css.groupProject}
                      data-testid="quickfind-group-project"
                      title={`${group.project} · ${group.hostName}`}
                    >
                      {group.project}
                    </span>
                    {group.branch ? (
                      <span className={css.groupBranch} data-testid="quickfind-group-branch">
                        {group.branch}
                      </span>
                    ) : null}
                    <span
                      className={css.groupBlocked}
                      data-testid="quickfind-group-blocked"
                      data-zero={group.blockedCount === 0 ? "1" : "0"}
                    >
                      {group.blockedCount} 待处理
                    </span>
                  </div>
                  {leaves.map(({ hit, index }) => (
                    <div key={hit.instance.id} data-index={index} className={css.optionSlot}>
                      <ResultRow
                        hit={hit}
                        index={index}
                        selected={index === active}
                        onSelect={() => choose(hit)}
                      />
                    </div>
                  ))}
                </div>
              ))
            : hits.map((hit, index) => (
                <div key={hit.instance.id} data-index={index} className={css.optionSlot}>
                  <ResultRow hit={hit} index={index} selected={index === active} onSelect={() => choose(hit)} />
                </div>
              ))}
        </div>
      ) : (
        <div className={css.empty} data-testid="quickfind-empty">
          <p>已加载的会话里没有匹配「{query.trim()}」的标题、空间或主机。</p>
          <p className={css.emptyHint}>不会搜索历史正文；清除搜索词可回到最近会话。</p>
          <button
            type="button"
            className={css.clear}
            data-testid="quickfind-clear"
            onClick={() => {
              setQuery("");
              inputRef.current?.focus();
            }}
          >
            清除搜索词
          </button>
        </div>
      )}
    </Sheet>
  );

  // On desktop the trigger lives inside the scrolling sidebar, so the scrim
  // portals to <body> and positions against the viewport. Inside the phone
  // Spaces drawer it must NOT portal: that drawer sits at z-index 45 while the
  // shared scrim is 40, so a portaled panel would render *under* the drawer
  // and every click would land on drawer content. Inline, the fixed scrim
  // paints inside the drawer's own stacking context instead.
  return inline ? panel : createPortal(panel, document.body);
}

/**
 * The panel entry point: a labelled search field in the /sessions index
 * column and the phone Spaces drawer. It carries the shortcut hint and is a
 * plain button — the finder owns no Space state.
 */
export function QuickFindTrigger() {
  return (
    <button
      type="button"
      className={css.trigger}
      data-testid="quickfind-trigger"
      aria-haspopup="dialog"
      title={`快速查找（${QUICKFIND_HINT}）`}
      onClick={() => openQuickFind()}
    >
      <span className={css.triggerIcon} aria-hidden="true">⌕</span>
      <span className={css.triggerText}>搜索所有空间…</span>
      <kbd className={css.kbd} aria-hidden="true">{QUICKFIND_HINT}</kbd>
    </button>
  );
}

function localStorageAccess(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
