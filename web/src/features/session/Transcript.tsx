import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useInRouterContext, useParams } from "react-router-dom";
import type { Observation } from "../../types/observation";
import { MarkdownText } from "../../components/MarkdownText";
import type { LocalBubble } from "../../lib/store";
import { SentAttachments } from "./AttachmentChips";
import { AnchorText } from "./AnchorText";
import { hubStore } from "../../lib/store";
import { projectCommandStatus } from "../../lib/commandStatus";
import ui from "../../styles/ui.module.css";
import { assembleTranscript, compactTranscript, isToolFailure, subagentToolNodes, type TranscriptNode } from "./assemble";
import { JournalBanner, type JournalUiStatus } from "./JournalBanner";
import { ToolCard } from "./ToolCard";
import { WorkflowTree } from "./WorkflowTree";
import { UsageFooter } from "./UsageFooter";
import { OpaqueRow } from "./OpaqueRow";
import { SubagentFolds } from "./subagent/SubagentRows";
import css from "./transcript.module.css";
import session from "./session.module.css";
import { DEFAULT_ROW, OVERSCAN, indexAtOffset, rowOffsets, visibleRange } from "./virtualWindow";
import { readShowInjected, writeShowInjected } from "./injectedPref";
import { readPosition, writePosition } from "./readingPosition";
import { findMatches, resolveSelection, type SearchMatch } from "./transcriptSearch";
import type { SteerHeldControl } from "../composer/state";
import type { MessageOrigin } from "../../types/generated";

/**
 * c-steer 插队发送 from a transcript held row. The page supplies it so the
 * composer's 已打断 receipt is raised on success; standalone callers fall back
 * to the store directly. Resolves `false` when the steer POST did not land.
 */
export type SteerHeldHandler = (
  instanceId: string,
  bubbleId: string,
) => Promise<boolean | void> | boolean | void;

/** Human-readable name for an injected origin, for the collapsed row. */
const ORIGIN_LABEL: Record<Exclude<MessageOrigin, "human">, string> = {
  "injected-skill": "skill",
  "injected-command-output": "命令输出",
  "hook-context": "hook 上下文",
  "tool-result": "工具结果",
  compaction: "对话压缩",
  unknown: "未知来源",
};

/**
 * Whether this node is text the human actually wrote.
 *
 * Assistant and system messages are never injections — only `user`-role
 * records can be, because that is the role Claude files injected text under.
 */
function isInjected(node: TranscriptNode): boolean {
  return node.type === "message" && node.role === "user" && node.origin !== "human";
}

/** Where a node id lives in the top-level list: directly or in a compact fold. */
type NodeLocation = { index: number; compactId: string | null };

function locateNode(nodes: readonly TranscriptNode[], nodeId: string): NodeLocation | null {
  for (let i = 0; i < nodes.length; i += 1) {
    const node = nodes[i];
    if (node.id === nodeId) return { index: i, compactId: null };
    if (node.type === "compact") {
      if (node.children.some((child) => child.id === nodeId)) {
        return { index: i, compactId: node.id };
      }
      // Subagent rows nested under a Task that the compact group swallowed.
      if (
        node.children.some(
          (child) => child.type === "tool" && subagentToolNodes(child).some((sub) => sub.id === nodeId),
        )
      ) {
        return { index: i, compactId: node.id };
      }
    }
    // Subagent tool rows are folded under Task / workflow member parents.
    if (
      node.type === "tool"
      && subagentToolNodes(node).some((child) => child.id === nodeId)
    ) {
      return { index: i, compactId: node.id };
    }
  }
  return null;
}

export function Transcript(props: {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
  journalStatus?: JournalUiStatus;
  onRetryJournal?: () => void;
  /** c-steer 插队发送 availability for the held rows shown in the transcript. */
  steerHeld?: SteerHeldControl;
  /** c-steer 插队发送 action for a held row; defaults to the store. */
  onSteerHeld?: SteerHeldHandler;
}) {
  // SessionPage mounts this inside a route; standalone unit tests do not.
  // useParams throws outside a Router, so only read it when one is present.
  const inRouter = useInRouterContext();
  if (inRouter) return <TranscriptWithRoute {...props} />;
  return <TranscriptInner {...props} routeInstanceId="" />;
}

function TranscriptWithRoute(props: {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
  journalStatus?: JournalUiStatus;
  onRetryJournal?: () => void;
  steerHeld?: SteerHeldControl;
  onSteerHeld?: SteerHeldHandler;
}) {
  const { instanceId = "" } = useParams();
  return <TranscriptInner {...props} routeInstanceId={instanceId} />;
}

function TranscriptInner({
  events,
  bubbles = [],
  compact = true,
  journalStatus = "live",
  onRetryJournal,
  routeInstanceId,
  steerHeld,
  onSteerHeld,
}: {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
  journalStatus?: JournalUiStatus;
  onRetryJournal?: () => void;
  routeInstanceId: string;
  steerHeld?: SteerHeldControl;
  onSteerHeld?: SteerHeldHandler;
}) {
  // Per-instance reading position/follow persistence. The id comes from the
  // route (read by the wrapper) so SessionPage needs no new prop; tests that
  // mount without a router simply get no persistence.
  const instanceId = routeInstanceId;
  const saved = useMemo(() => (instanceId ? readPosition(instanceId) : null), [instanceId]);

  // The route keeps this component mounted while the reader moves directly
  // between sessions (tab switch). Per-instance refs must therefore reset on
  // an id change, or one session's "already restored"/pin state would leak
  // into the next. The reset runs during render (not in an effect) so React
  // StrictMode's dev-time mount replay cannot wipe a restore set by the
  // passive restore effect.
  const [instanceEpoch, setInstanceEpoch] = useState(instanceId);
  const restoredRef = useRef(false);
  // Follow state restores from the last visit; a brand-new session pins.
  const pinRef = useRef(saved ? saved.follow : true);
  const scrollTopRef = useRef(0);
  const saveTimer = useRef<number | null>(null);
  const pendingScroll = useRef<
    | { kind: "index"; index: number; offset: number; tries: number }
    | { kind: "restore"; anchorId: string; offset: number; tries: number }
    | null
  >(null);
  // c-steer 插队发送 in-flight latch, mirroring the composer chip row: a double
  // click on a held transcript row posts exactly once.
  const steeringRef = useRef<Set<string>>(new Set());
  if (instanceEpoch !== instanceId) {
    setInstanceEpoch(instanceId);
    restoredRef.current = false;
    pinRef.current = saved ? saved.follow : true;
    scrollTopRef.current = 0;
    pendingScroll.current = null;
    steeringRef.current.clear();
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
  }

  const [showInjected, setShowInjected] = useState(readShowInjected);
  const assembled = useMemo(
    () => compactTranscript(assembleTranscript(events, bubbles), compact),
    [events, bubbles, compact],
  );
  // Injected records are dropped from the list rather than hidden with CSS so
  // the virtual window measures the rows it actually draws.
  const nodes = useMemo(
    () => (showInjected ? assembled : assembled.filter((node) => !isInjected(node))),
    [assembled, showInjected],
  );
  const injectedCount = useMemo(() => assembled.filter(isInjected).length, [assembled]);
  const [collapseTick, setCollapseTick] = useState(0);
  const [activeTurn, setActiveTurn] = useState<string | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewport, setViewport] = useState(720);
  const [sizes, setSizes] = useState<number[]>([]);
  const scrollerRef = useRef<HTMLDivElement>(null);
  // Latest scroll offset in a ref: passive-effect cleanup runs after refs are
  // detached on unmount, so the leave-session flush cannot read the DOM.
  const viewportRef = useRef(720);
  // Per-row height used for geometry the window has not measured yet. It is
  // seeded from the last visit's measured average and converges to this
  // visit's average as rows mount; without it, restoring a position deep in a
  // 2,000-row journal drifts by the difference between the 96px placeholder
  // and real row heights.
  const [estimate, setEstimate] = useState(saved && saved.avgRow > 0 ? saved.avgRow : DEFAULT_ROW);
  const estimateRef = useRef(estimate);
  useLayoutEffect(() => {
    estimateRef.current = estimate;
  }, [estimate]);
  const nodesRef = useRef(nodes);
  const sizesHold = useRef(sizes);
  // saveTimer and pendingScroll are declared with the other per-instance
  // refs above so the route-switch reset can clear them.
  // A scroll request toward a row whose size the window has not measured yet
  // (a search hit, j/k navigation, or a saved anchor) is refined as
  // ResizeObserver reports the real heights — see the pendingScroll effect.
  useEffect(() => {
    sizesHold.current = sizes;
  }, [sizes]);

  // --- in-transcript search -------------------------------------------------
  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selectedIdx, setSelectedIdx] = useState(-1);
  const lastMatchRef = useRef<SearchMatch | null>(null);
  const openButtonRef = useRef<HTMLButtonElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // Bumped to force the compact fold holding the current hit open.
  const [expandTick, setExpandTick] = useState(0);
  const [compactHit, setCompactHit] = useState<{ compactId: string; childId: string } | null>(null);

  const matches = useMemo(() => (searchOpen ? findMatches(nodes, query) : []), [nodes, query, searchOpen]);

  // Keep the current hit on its node while streaming appends/revisions
  // rebuild the list, instead of letting index N point at a different node.
  useEffect(() => {
    if (!searchOpen) return;
    setSelectedIdx((current) => {
      const previous = current >= 0 ? matches[current] : null;
      return resolveSelection(matches, previous ?? lastMatchRef.current);
    });
  }, [matches, searchOpen]);
  const currentMatch: SearchMatch | null = selectedIdx >= 0 ? matches[selectedIdx] ?? null : null;
  useEffect(() => {
    lastMatchRef.current = currentMatch;
  }, [currentMatch]);

  const hitNodes = useMemo(() => {
    const map = new Map<string, { current: boolean; childId: string | null }>();
    matches.forEach((match, i) => {
      const located = locateNode(nodes, match.nodeId);
      if (!located) return;
      const topId = nodes[located.index].id;
      const prev = map.get(topId);
      map.set(topId, {
        current: i === selectedIdx || Boolean(prev?.current),
        childId: located.compactId ? match.nodeId : prev?.childId ?? null,
      });
    });
    return map;
  }, [matches, nodes, selectedIdx]);

  const settle = journalStatus !== "gap-backfill";
  const defaultFolded = collapseTick > 0;
  const range = useMemo(
    () => visibleRange(nodes.length, sizes, scrollTop, viewport, OVERSCAN, estimate),
    [nodes.length, sizes, scrollTop, viewport, estimate],
  );

  const setRowSize = useCallback((index: number, height: number) => {
    if (height <= 0) return;
    setSizes((prev) => {
      const last = prev[index] ?? 0;
      if (Math.abs(last - height) < 1) return prev;
      const next = prev.slice();
      next[index] = height;
      return next;
    });
  }, []);

  // Converge the unmeasured-row estimate on this visit's real average so
  // offsets outside the window (search hits, saved position) stop drifting.
  // While a saved position is being restored, the estimate is frozen at the
  // previous visit's average: the rows above the anchor were estimates at
  // save time too, so freezing reproduces their contribution exactly instead
  // of biasing the total toward the rows this window happened to mount.
  const restoringRef = useRef(false);
  useLayoutEffect(() => {
    if (restoringRef.current) return;
    const measured = sizes.filter((h) => h > 0);
    if (measured.length < 6) return;
    const avg = measured.reduce((sum, h) => sum + h, 0) / measured.length;
    setEstimate((prev) => (Math.abs(prev - avg) / prev > 0.03 ? avg : prev));
  }, [sizes]);

  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const measure = () => {
      const next = el.clientHeight;
      const viewport = next < 32 ? 720 : next;
      viewportRef.current = viewport;
      setViewport(viewport);
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    nodesRef.current = nodes;
  }, [nodes]);

  const applyOffset = useCallback((index: number, offset: number) => {
    const el = scrollerRef.current;
    if (!el) return;
    const { offsets, total } = rowOffsets(nodesRef.current.length, sizesHold.current, estimateRef.current);
    const base = offsets[index] ?? 0;
    const max = Math.max(0, total - el.clientHeight);
    const top = Math.min(max, Math.max(0, base + offset));
    el.scrollTop = top;
    scrollTopRef.current = top;
  }, []);

  // Refine an estimated scroll (search hit, saved position) as the window
  // measures the rows around it. pendingScroll is declared with the other
  // per-instance refs so the route-switch reset can clear it.
  useLayoutEffect(() => {
    const el = scrollerRef.current;
    const pending = pendingScroll.current;
    if (!el || !pending || !nodesRef.current.length) return;
    if (pending.kind === "restore") {
      // Estimate a starting position from the saved average, then correct
      // against the anchor's real DOM position once the window mounts it.
      // Pure estimate math drifts because the sets of already-measured rows
      // differ between visits (follow opens at the bottom, a restored visit
      // opens at the top); a DOM-relative correction is independent of which
      // other rows happen to have been measured.
      const rowEl = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(pending.anchorId)}"]`);
      if (rowEl) {
        const delta = rowEl.getBoundingClientRect().top - el.getBoundingClientRect().top - pending.offset;
        if (Math.abs(delta) <= 2) {
          pendingScroll.current = null;
          restoringRef.current = false;
          return;
        }
        el.scrollTop += delta;
        scrollTopRef.current = el.scrollTop;
      } else {
        const index = nodesRef.current.findIndex((n) => n.id === pending.anchorId);
        if (index >= 0) {
          const { offsets } = rowOffsets(nodesRef.current.length, sizesHold.current, estimateRef.current);
          const top = Math.round((offsets[index] ?? 0) + pending.offset);
          el.scrollTop = top;
          scrollTopRef.current = top;
        }
      }
      pending.tries += 1;
      if (pending.tries >= 24) {
        pendingScroll.current = null;
        restoringRef.current = false;
      }
      return;
    }
    const measured = (sizesHold.current[pending.index] ?? 0) > 0;
    applyOffset(pending.index, pending.offset);
    if (measured || pending.tries >= 8) {
      pendingScroll.current = null;
      return;
    }
    pending.tries += 1;
  }, [sizes, nodes, estimate, applyOffset]);

  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el || !pinRef.current) return;
    el.scrollTop = el.scrollHeight;
    scrollTopRef.current = el.scrollTop;
  }, [nodes.length, sizes]);

  const flushPosition = useCallback((top: number) => {
    if (!instanceId) return;
    const list = nodesRef.current;
    if (!list.length) return;
    const est = estimateRef.current;
    const { offsets, total } = rowOffsets(list.length, sizesHold.current, est);
    const scrollable = Math.max(0, total - viewportRef.current);
    const index = Math.min(indexAtOffset(list.length, sizesHold.current, top, est), list.length - 1);
    const anchorId = list[index]?.id;
    if (!anchorId) return;
    writePosition(instanceId, {
      anchorId,
      offset: Math.max(0, top - (offsets[index] ?? 0)),
      ratio: scrollable > 0 ? Math.min(1, Math.max(0, top / scrollable)) : 0,
      avgRow: est,
      follow: pinRef.current,
    });
  }, [instanceId]);

  const persistSoon = useCallback(() => {
    if (!instanceId) return;
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    const top = scrollTopRef.current;
    saveTimer.current = window.setTimeout(() => flushPosition(top), 250);
  }, [instanceId, flushPosition]);

  // Restore the saved reading position once nodes are available, then leave
  // the transcript to normal pin/scroll behavior.
  useEffect(() => {
    if (restoredRef.current || !saved || saved.follow || !nodes.length) return;
    const located = locateNode(nodes, saved.anchorId);
    if (!located) return;
    pinRef.current = false;
    restoringRef.current = true;
    pendingScroll.current = { kind: "restore", anchorId: saved.anchorId, offset: saved.offset, tries: 0 };
    restoredRef.current = true;
  }, [nodes, saved]);

  // Leaving the session: flush whatever the debounce has not written. The DOM
  // ref is already detached when passive cleanup runs, so use the tracked
  // offset and node/size refs instead of reading the element.
  useEffect(() => {
    return () => {
      if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
      flushPosition(scrollTopRef.current);
    };
  }, [flushPosition]);

  const scrollToIndex = useCallback((index: number) => {
    const el = scrollerRef.current;
    if (!el || index < 0) return;
    pinRef.current = index >= nodesRef.current.length - 1;
    pendingScroll.current = { kind: "index", index, offset: 0, tries: 0 };
    applyOffset(index, 0);
  }, [applyOffset]);

  const turnIds = useMemo(
    () => nodes.filter((n) => n.type === "message").map((n) => n.id),
    [nodes],
  );

  const openSearch = useCallback(() => {
    setSearchOpen(true);
    requestAnimationFrame(() => inputRef.current?.focus());
  }, []);

  const closeSearch = useCallback(() => {
    setSearchOpen(false);
    setQuery("");
    setSelectedIdx(-1);
    setCompactHit(null);
    openButtonRef.current?.focus();
  }, []);

  const gotoMatch = useCallback((next: number) => {
    const match = matches[next];
    if (!match) return;
    setSelectedIdx(next);
    const located = locateNode(nodesRef.current, match.nodeId);
    if (!located) return;
    pinRef.current = false;
    if (located.compactId) {
      setCompactHit({ compactId: located.compactId, childId: match.nodeId });
      setExpandTick((n) => n + 1);
    } else {
      setCompactHit(null);
    }
    pendingScroll.current = { kind: "index", index: located.index, offset: 0, tries: 0 };
    applyOffset(located.index, 0);
  }, [matches, applyOffset]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const editable = Boolean(target?.closest("textarea, input, select, [contenteditable='true']"));
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "f" && !event.altKey) {
        // Browser find cannot see outside the virtual window; own it.
        event.preventDefault();
        openSearch();
        return;
      }
      if (searchOpen && event.key === "Escape") {
        event.preventDefault();
        closeSearch();
        return;
      }
      if (!searchOpen && event.key === "/" && !editable && !event.metaKey && !event.ctrlKey && !event.altKey && !event.isComposing) {
        event.preventDefault();
        openSearch();
        return;
      }
      if (editable) return;
      if (target?.closest(".xterm")) return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key !== "j" && event.key !== "k") return;
      event.preventDefault();
      const ids = turnIds;
      if (!ids.length) return;
      const current = activeTurn ? ids.indexOf(activeTurn) : -1;
      const nextIndex =
        event.key === "j"
          ? Math.min(ids.length - 1, current < 0 ? 0 : current + 1)
          : Math.max(0, current < 0 ? ids.length - 1 : current - 1);
      const nextId = ids[nextIndex];
      setActiveTurn(nextId);
      pinRef.current = false;
      scrollToIndex(nodesRef.current.findIndex((n) => n.id === nextId));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activeTurn, turnIds, scrollToIndex, searchOpen, openSearch, closeSearch]);

  const atBottom = range.total - scrollTop - viewport < 64;
  const showJump = nodes.length > 0 && !atBottom;
  const slice = nodes.slice(range.start, range.end);

  return (
    <div className={css.root} data-testid="transcript" aria-live="off">
      <JournalBanner status={journalStatus} onRetry={onRetryJournal} />
      <div className={css.toolbar}>
        <button type="button" className={ui.chip} data-testid="collapse-all" onClick={() => setCollapseTick((n) => n + 1)}>
          全部折叠
        </button>
        <button
          ref={openButtonRef}
          type="button"
          className={ui.chip}
          data-testid="transcript-search-open"
          aria-expanded={searchOpen}
          onClick={() => (searchOpen ? closeSearch() : openSearch())}
        >
          搜索正文
        </button>
        {injectedCount > 0 ? (
          <button
            type="button"
            className={ui.chip}
            data-testid="toggle-injected"
            aria-pressed={showInjected}
            onClick={() => {
              const next = !showInjected;
              setShowInjected(next);
              writeShowInjected(next);
            }}
          >
            {showInjected ? `隐藏注入内容 · ${injectedCount}` : `显示注入内容 · ${injectedCount}`}
          </button>
        ) : null}
      </div>
      {searchOpen ? (
        <div className={css.searchbar} role="search" aria-label="正文搜索">
          <input
            ref={inputRef}
            className={css.searchInput}
            data-testid="transcript-search-input"
            type="text"
            value={query}
            placeholder="搜索已加载正文（Enter 下一项，Shift+Enter 上一项）"
            onChange={(event) => {
              setQuery(event.target.value);
              setSelectedIdx(-1);
              lastMatchRef.current = null;
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                if (!matches.length) return;
                const delta = event.shiftKey ? -1 : 1;
                const base = selectedIdx < 0 ? (delta === 1 ? -1 : 0) : selectedIdx;
                gotoMatch((base + delta + matches.length) % matches.length);
              }
            }}
          />
          <span className={css.searchCount} data-testid="transcript-search-count" aria-live="off">
            {query.trim() ? (matches.length ? `${selectedIdx < 0 ? 0 : selectedIdx + 1}/${matches.length}` : `0/${matches.length}`) : "0/0"}
          </span>
          <button
            type="button"
            className={ui.chip}
            data-testid="transcript-search-prev"
            disabled={!matches.length}
            onClick={() => gotoMatch((selectedIdx - 1 + matches.length) % matches.length)}
          >
            上一项
          </button>
          <button
            type="button"
            className={ui.chip}
            data-testid="transcript-search-next"
            disabled={!matches.length}
            onClick={() => gotoMatch((selectedIdx + 1) % matches.length)}
          >
            下一项
          </button>
          <button type="button" className={ui.chip} data-testid="transcript-search-close" onClick={closeSearch}>
            退出
          </button>
        </div>
      ) : null}
      <div
        ref={scrollerRef}
        className={css.scroller}
        data-testid="transcript-scroller"
        onScroll={(event) => {
          const el = event.currentTarget;
          setScrollTop(el.scrollTop);
          scrollTopRef.current = el.scrollTop;
          pinRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 64;
          persistSoon();
        }}
      >
        <div className={css.list}>
          <div style={{ height: range.padTop }} aria-hidden />
          {slice.map((node, i) => {
            const index = range.start + i;
            const hit = hitNodes.get(node.id);
            return (
              <TranscriptRow
                key={node.id}
                node={node}
                index={index}
                active={activeTurn === node.id}
                defaultFolded={defaultFolded}
                collapseTick={collapseTick}
                settle={settle}
                onSize={setRowSize}
                searchHit={Boolean(hit)}
                searchCurrent={Boolean(hit?.current)}
                instanceId={instanceId}
                steerHeld={steerHeld}
                onSteerHeld={onSteerHeld}
                steering={steeringRef.current}
                expandTick={node.type === "compact" && compactHit?.compactId === node.id ? expandTick : 0}
                hitChildId={
                  node.type === "compact"
                    ? (compactHit?.childId ?? null)
                    : node.type === "tool"
                      ? (hit?.childId ?? null)
                      : null
                }
              />
            );
          })}
          <div style={{ height: range.padBottom }} aria-hidden />
        </div>
      </div>
      {showJump ? (
        <button
          type="button"
          className={`${ui.chip} ${css.jump}`}
          data-testid="jump-latest"
          onClick={() => {
            pinRef.current = true;
            const el = scrollerRef.current;
            if (el) el.scrollTop = el.scrollHeight;
            const last = turnIds[turnIds.length - 1];
            if (last) setActiveTurn(last);
            persistSoon();
          }}
        >
          跳到最新
        </button>
      ) : null}
    </div>
  );
}

function TranscriptRow({
  node,
  index,
  active,
  defaultFolded,
  collapseTick,
  settle,
  onSize,
  searchHit,
  searchCurrent,
  expandTick,
  hitChildId,
  instanceId,
  steerHeld,
  onSteerHeld,
  steering,
}: {
  node: TranscriptNode;
  index: number;
  active: boolean;
  defaultFolded: boolean;
  collapseTick: number;
  settle: boolean;
  onSize: (index: number, height: number) => void;
  searchHit: boolean;
  searchCurrent: boolean;
  expandTick: number;
  hitChildId: string | null;
  instanceId: string;
  steerHeld?: SteerHeldControl;
  onSteerHeld?: SteerHeldHandler;
  steering: Set<string>;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const report = () => onSize(index, el.getBoundingClientRect().height + 12);
    report();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(report);
    ro.observe(el);
    return () => ro.disconnect();
  }, [index, onSize, node, collapseTick, expandTick]);
  return (
    <div
      ref={ref}
      id={`t-${node.id}`}
      data-testid="transcript-row"
      data-anchor={node.id}
      data-kind={node.type}
      data-role={node.type === "message" ? node.role : undefined}
      // C2 correlation evidence: the Node joins command-delivered prompts
      // onto the delivering command; optimistic bubbles carry their
      // server-assigned id once the POST returns.
      data-command-id={
        node.type === "message"
          ? (node.commandId ??
            (typeof node.local?.commandId === "string" ? node.local.commandId : undefined))
          : undefined
      }
      data-turn-active={active ? "1" : "0"}
      data-search-hit={searchHit ? "1" : "0"}
      data-search-current={searchCurrent ? "1" : "0"}
      className={[
        css.row,
        active ? css.rowActive : "",
        searchHit ? css.rowHit : "",
        searchCurrent ? css.rowCurrent : "",
      ].join(" ").trim()}
    >
      {renderNode(node, { defaultFolded, collapseTick, settle, expandTick, hitChildId, instanceId, steerHeld, onSteerHeld, steering })}    </div>
  );
}

/** A failed tool gets a visible inline tag and never obeys collapse-all. */
function ToolRow({
  node,
  opts,
  hitChildId,
}: {
  node: Extract<TranscriptNode, { type: "tool" }>;
  opts: { defaultFolded: boolean; collapseTick: number; settle: boolean };
  hitChildId?: string | null;
}): ReactNode {
  const failed = isToolFailure(node);
  const card = (
    <ToolCard
      key={`${node.id}:${opts.collapseTick}`}
      driverKind={node.driverKind}
      call={node.call}
      result={node.result}
      completeness={node.completeness}
      diffState={node.diffState}
      workflow={node.workflow}
      defaultFolded={failed ? false : opts.defaultFolded}
      settle={opts.settle}
    />
  );
  const folds = (node.subagents?.length ?? 0) > 0 ? (
    <SubagentFolds refs={node.subagents ?? []} openChildId={hitChildId ?? null} />
  ) : null;
  if (!failed) {
    return (
      <>
        {card}
        {folds}
      </>
    );
  }
  return (
    <div className={css.failWrap} data-testid="tool-failure" data-tool-outcome={node.result?.outcome}>
      <span className={css.failTag} data-testid="tool-failure-tag">
        工具失败 · {node.result?.outcome === "denied" ? "已拒绝" : "失败"}
      </span>
      {card}
      {folds}
    </div>
  );
}

function renderNode(
  node: TranscriptNode,
  opts: {
    defaultFolded: boolean;
    collapseTick: number;
    settle: boolean;
    expandTick?: number;
    hitChildId?: string | null;
    instanceId?: string;
    steerHeld?: SteerHeldControl;
    onSteerHeld?: SteerHeldHandler;
    steering: Set<string>;
  },
): ReactNode {
  if (node.type === "message") {
    const user = node.role === "user";
    // Injected text keeps its place in the turn but is collapsed to a muted
    // row: it is not the user's words, and drawing it as a "You" bubble is
    // what made people think they had sent it themselves.
    if (user && node.origin !== "human") {
      const kind = ORIGIN_LABEL[node.origin] ?? node.origin;
      return (
        <details className={session.thought} data-testid="injected-row" data-origin={node.origin}>
          <summary>
            系统注入 · {kind} · {node.text.length} 字
          </summary>
          <p className={session.bubble}>{node.text}</p>
        </details>
      );
    }
    const streaming = node.status === "streaming";
    // C2: the bubble speaks the C1 vocabulary projected from its own facts
    // (null commandId → 状态待确认, queued → 等待发送), never the raw state.
    const localRow = node.local
      ? projectCommandStatus({
          hasServerCommandId: node.local.commandId !== null,
          localState: node.local.state,
        })
      : null;
    return (
      <section
        className={user ? session.user : session.assistant}
        data-testid={node.local ? "optimistic-bubble" : "message"}
        data-status={node.status}
        data-held={node.local?.held ? node.holdReason ?? "turn" : undefined}
        data-command-id={node.local?.commandId ?? undefined}
      >
        <div className={session.you}>
          {user ? "You" : node.role}
          {/* c-steer: held queue rows show why they wait and their queue
              position; delivered (POSTed) rows lose the tag entirely. */}
          {node.local?.held ? (
            <span
              className={session.stat}
              data-testid="held-queue-tag"
              data-ordinal={node.holdOrdinal ?? undefined}
            >
              {node.holdReason === "answer"
                ? " · 待回答后送出"
                : ` · 排队中 · 第 ${node.holdOrdinal ?? 1} 条 · 回车后送出`}
            </span>
          ) : localRow ? (
            ` · ${localRow.label}`
          ) : null}
          {/* Status order the composer and transcript share: a queued
              journal node has not been sent; the local bubble's own wording
              comes from the projected row above. */}
          {node.status === "queued" && !node.local ? <span className={session.stat}> · 排队中</span> : null}
          {node.status === "interrupted" ? <span className={session.stat}> · 已打断</span> : null}
          {/* c-steer: a delivered 插队 row keeps a small badge even after the
              queued tag drops, so the reader knows the turn was interrupted. */}
          {!node.local && (node as { promptMode?: string }).promptMode === "steer" ? (
            <span className={session.stat} data-testid="steer-delivered-tag">
              {" "}· 插队
            </span>
          ) : null}
        </div>
        {node.role === "assistant" ? (
          <MarkdownText text={node.text} />
        ) : node.local ? (
          // Optimistic local bubble: inline [Image #n] thumbnails can render
          // from the staged previews. Journaled user messages keep plain text
          // until the Hub echoes attachments on the journal.
          <AnchorText
            text={node.text}
            attachments={node.local.attachments}
            className={session.bubble}
          />
        ) : node.localAttachments?.length ? (
          // Journal node joined onto its optimistic bubble by commandId:
          // inline [Image #n] anchors resolve against the staged previews.
          <AnchorText
            text={node.text}
            attachments={node.localAttachments}
            className={session.bubble}
          />
        ) : (
          <p className={session.bubble}>{node.text}</p>
        )}
        {streaming ? <span className={session.cursor} data-testid="streaming-cursor" aria-hidden /> : null}
        {node.local?.attachments?.length || node.localAttachments?.length ? (
          <SentAttachments attachments={(node.local?.attachments ?? node.localAttachments)!} />
        ) : null}
        {node.local?.state === "queued" ? (
          <div className={session.heldRowActions} data-testid="held-row-actions">
            {node.local.held ? (
              <button
                type="button"
                className={ui.chip}
                data-testid="held-queue-steer"
                disabled={!opts.steerHeld?.enabled}
                aria-label={
                  opts.steerHeld?.enabled
                    ? "插队发送这条排队消息"
                    : `插队发送这条排队消息（不可用：${opts.steerHeld?.reason ?? "无进行中的回合，回车即送出"}）`
                }
                title={
                  opts.steerHeld?.enabled
                    ? opts.steerHeld.reason
                      ? `插队发送，打断当前 turn 并立即发送（${opts.steerHeld.reason}）`
                      : "插队发送，打断当前 turn 并立即发送"
                    : opts.steerHeld?.reason ?? "无进行中的回合，回车即送出"
                }
                onClick={() => {
                  if (!opts.steerHeld?.enabled || !opts.instanceId) return;
                  const bubbleId = node.local!.clientRequestId;
                  // Same in-flight latch as the composer chip row: a double
                  // click posts exactly once while the row is still queued.
                  if (opts.steering.has(bubbleId)) return;
                  opts.steering.add(bubbleId);
                  // The page handler raises the composer's 已打断 receipt; the
                  // standalone fallback drives the store directly.
                  const steer =
                    opts.onSteerHeld ??
                    ((iid: string, bid: string) => hubStore.steerHeld(iid, bid));
                  void steer(opts.instanceId, bubbleId);
                }}
              >
                插队发送
              </button>
            ) : null}
            <button
              className={ui.chip}
              data-testid="held-queue-cancel"
              onClick={() => hubStore.retract(node.local!.clientRequestId)}
            >
              {node.local.held ? "取消排队" : "撤回"}
            </button>
          </div>
        ) : null}
        {node.local?.state === "unknown" ? (
          <button className={ui.chip} onClick={() => void hubStore.send(node.local!.instanceId, node.local!.text)}>
            仍要再送一条？
          </button>
        ) : null}
      </section>
    );
  }
  if (node.type === "thought") {
    return (
      <details className={session.thought}>
        <summary>▸ thinking{node.completeness === "screen-derived" ? " · 从屏幕猜测" : ""}</summary>
        <p>{node.text}</p>
      </details>
    );
  }
  if (node.type === "tool") {
    return <ToolRow node={node} opts={opts} hitChildId={opts.hitChildId ?? null} />;
  }
  if (node.type === "workflow") {
    return <WorkflowTree run={node.run} phases={node.phases} members={node.members} />;
  }
  if (node.type === "usage") {
    return <UsageFooter payload={node.payload} />;
  }
  if (node.type === "compact") {
    return (
      <CompactFold
        toolCount={node.toolCount}
        thoughtCount={node.thoughtCount}
        expandTick={opts.expandTick ?? 0}
        hitChildId={opts.hitChildId ?? null}
      >
        {node.children.map((child) =>
          child.type === "tool" ? (
            <ToolRow
              key={child.id}
              node={child}
              opts={{ defaultFolded: opts.defaultFolded, collapseTick: opts.collapseTick, settle: opts.settle }}
              hitChildId={opts.hitChildId ?? null}
            />
          ) : child.type === "thought" ? (
            <details key={child.id} className={session.thought}>
              <summary>thinking</summary>
              <p>{child.text}</p>
            </details>
          ) : null,
        )}
      </CompactFold>
    );
  }
  if (node.type === "error") {
    return (
      <article className={ui.card}>
        <div className={ui.cardHead}>
          <strong>error</strong>
        </div>
        <p>{node.text}</p>
      </article>
    );
  }
  if (node.type === "opaque") {
    return <OpaqueRow kind={node.kind} summary={node.summary} raw={node.raw} />;
  }
  return null;
}

function CompactFold({
  toolCount,
  thoughtCount,
  expandTick,
  hitChildId,
  children,
}: {
  toolCount: number;
  thoughtCount: number;
  expandTick: number;
  hitChildId: string | null;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  // A search hit inside the fold opens it and keeps it open.
  useEffect(() => {
    if (expandTick > 0) setOpen(true);
  }, [expandTick]);
  return (
    <div data-testid="compact-fold-wrap" data-hit-child={hitChildId ?? undefined}>
      <button className={session.fold} data-testid="compact-fold" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span>▸</span>
        <span>{open ? "收起过程" : `${toolCount} 次工具 · ${thoughtCount} 段思考`}</span>
      </button>
      {open ? <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>{children}</div> : null}
    </div>
  );
}
