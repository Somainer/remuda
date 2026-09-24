import {
  Profiler,
  forwardRef,
  memo,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ForwardedRef,
  type ProfilerOnRenderCallback,
  type ReactNode,
} from "react";
import { useInRouterContext, useParams } from "react-router-dom";
import type { Observation } from "../../types/observation";
import { MarkdownText } from "../../components/MarkdownText";
import { balanceFences } from "../../components/fenceBalance";
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
import { ObservedChangeRow } from "./ObservedChangeRow";
import { SubagentFolds } from "./subagent/SubagentRows";
import css from "./transcript.module.css";
import session from "./toolCard.module.css";
import { DEFAULT_ROW, OVERSCAN, indexAtOffset, rowOffsets, visibleRange } from "./virtualWindow";
import { readShowInjected, writeShowInjected } from "./injectedPref";
import { readPosition, writePosition } from "./readingPosition";
import { dismissWorkflow, readDismissedWorkflows, undismissWorkflow } from "./workflowDismiss";
import { findMatches, resolveSelection, type SearchMatch } from "./transcriptSearch";
import type { SteerHeldControl } from "../composer/state";
import type { MessageOrigin } from "../../types/generated";
import { COMPACT_WORKBENCH_QUERY } from "../../lib/viewport";
import { profileRegion, profilingEnabled, reportProbe } from "../../lib/profileFlags";

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

/**
 * Whether the workbench is in the compact (mobile) layout. Mirrors the hook
 * ToolCard owns for the D-041 fold: the toolbar fold (D-049) is a layout
 * question, not the `compact` density prop, and a missing matchMedia (unit
 * DOM) reads as the desktop default where chips stay inline.
 */
function useCompactLayout(): boolean {
  const read = () =>
    typeof window !== "undefined" && typeof window.matchMedia === "function"
      ? window.matchMedia(COMPACT_WORKBENCH_QUERY).matches
      : false;
  const [compact, setCompact] = useState(read);
  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const media = window.matchMedia(COMPACT_WORKBENCH_QUERY);
    const update = () => setCompact(media.matches);
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return compact;
}

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

/** Imperative actions a page may drive from its own header controls. */
export type TranscriptHandle = {
  /** Open the in-transcript search bar and focus its input. */
  openSearch(): void;
  /** Fold every tool card that is not a failure (same as 全部折叠). */
  collapseAll(): void;
};

export type TranscriptProps = {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
  journalStatus?: JournalUiStatus;
  onRetryJournal?: () => void;
  /** c-steer 插队发送 availability for the held rows shown in the transcript. */
  steerHeld?: SteerHeldControl;
  /** c-steer 插队发送 action for a held row; defaults to the store. */
  onSteerHeld?: SteerHeldHandler;
  /**
   * Whether the transcript draws its own action strip (全部折叠 / 搜索正文 /
   * 注入内容). A page that moves these actions into its header passes false
   * and drives them through TranscriptHandle instead.
   */
  toolbar?: boolean;
};

type InnerProps = TranscriptProps & {
  routeInstanceId: string;
  handleRef: ForwardedRef<TranscriptHandle>;
};

export const Transcript = forwardRef<TranscriptHandle, TranscriptProps>(function Transcript(props, ref) {
  // SessionPage mounts this inside a route; standalone unit tests do not.
  // useParams throws outside a Router, so only read it when one is present.
  const inRouter = useInRouterContext();
  if (inRouter) return <TranscriptWithRoute {...props} handleRef={ref} />;
  return <TranscriptInner {...props} routeInstanceId="" handleRef={ref} />;
});

function TranscriptWithRoute(props: Omit<InnerProps, "routeInstanceId">) {
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
  toolbar = true,
  handleRef,
}: InnerProps) {
  // Per-instance reading position/follow persistence. The id comes from the
  // route (read by the wrapper) so SessionPage needs no new prop; tests that
  // mount without a router simply get no persistence.
  const instanceId = routeInstanceId;
  const saved = useMemo(() => (instanceId ? readPosition(instanceId) : null), [instanceId]);

  // c-wfcard: workflow ids whose live card this reader dismissed. Dismissal is
  // per workflow and persisted (runtime localStorage); only a dismissed card
  // may be swept into the compact fold. The Set itself is the state; the
  // instance-switch reset below re-reads it for the new route id.
  const [dismissedWorkflows, setDismissedWorkflows] = useState<Set<string>>(() =>
    instanceId ? readDismissedWorkflows(instanceId) : new Set<string>(),
  );

  // D-041: node ids whose folded tool row the reader expanded. The transcript
  // virtualises rows (rows unmount ~8 rows out of the window), so expansion
  // cannot live in the card's own useState or the row height collapses again
  // on the way back. Session-scoped like dismissedWorkflows; 全部折叠 clears
  // it explicitly (collapse must re-fold even a previously expanded row).
  const [expandedTools, setExpandedTools] = useState<Set<string>>(new Set());

  // Bounded-window paging: true while the load-earlier row awaits its page.
  // Declared before the route-switch reset below, which clears it per
  // instance like the other per-route refs.
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  // Row heights keyed by STABLE NODE ID, never array index (see the `sizes`
  // memo below); reset per session in the route-reset block.
  const [rowHeights, setRowHeights] = useState<Map<string, number>>(new Map());

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
  // Held anchor for a load-earlier prepend; repinned across post-prepend
  // estimate/size changes (see the effect near the scroll math).
  const prependAnchorRef = useRef<{ anchorId: string; offset: number; tries: number } | null>(null);
  // c-steer 插队发送 in-flight latch, mirroring the composer chip row: a double
  // click on a held transcript row posts exactly once.
  const steeringRef = useRef<Set<string>>(new Set());
  if (instanceEpoch !== instanceId) {
    setInstanceEpoch(instanceId);
    restoredRef.current = false;
    pinRef.current = saved ? saved.follow : true;
    scrollTopRef.current = 0;
    pendingScroll.current = null;
    prependAnchorRef.current = null;
    steeringRef.current.clear();
    setLoadingEarlier(false);
    setRowHeights(new Map());
    setExpandedTools(new Set());
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    setDismissedWorkflows(instanceId ? readDismissedWorkflows(instanceId) : new Set());
  }

  const [showInjected, setShowInjected] = useState(readShowInjected);
  const toggleToolExpand = useCallback((nodeId: string, expanded: boolean) => {
    setExpandedTools((prev) => {
      if (expanded === prev.has(nodeId)) return prev;
      const next = new Set(prev);
      if (expanded) next.add(nodeId);
      else next.delete(nodeId);
      return next;
    });
  }, []);
  const toggleWorkflowDismiss = useCallback(
    (workflowId: string, dismiss: boolean) => {
      if (!instanceId) return;
      const next = dismiss
        ? dismissWorkflow(instanceId, workflowId)
        : undismissWorkflow(instanceId, workflowId);
      setDismissedWorkflows(new Set(next));
    },
    [instanceId],
  );
  const assembled = useMemo(
    () =>
      // c-perfaudit: when ?profile=1, attributes the long task covering this
      // rebuild to the transcript assembly region. Zero-opts otherwise.
      profileRegion("transcript.assemble", () =>
        compactTranscript(assembleTranscript(events, bubbles), compact, dismissedWorkflows),
      ),
    [events, bubbles, compact, dismissedWorkflows],
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
  // Derive the index-keyed size array the virtual window reads from. The
  // heights map above is keyed by STABLE NODE ID: load-earlier prepends
  // thousands of nodes without shifting index-keyed measurements, so padTop
  // for the held anchor cannot jump.
  const sizes = useMemo(
    () => nodes.map((node) => rowHeights.get(node.id) ?? 0),
    [nodes, rowHeights],
  );
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
  // D-049 compact chrome budget: on a compact layout the toolbar's action
  // chips fold behind one ⋯ trigger; expanding mounts the same buttons with
  // the same testids, and choosing an action folds the row back so at most
  // one toolbar strip is ever visible.
  const compactLayout = useCompactLayout();
  const [toolsOpen, setToolsOpen] = useState(false);
  const toolsFolded = compactLayout && !toolsOpen;
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

  const currentHitIds = useMemo(() => {
    const set = new Set<string>();
    const match = matches[selectedIdx];
    if (!match) return set;
    const located = locateNode(nodes, match.nodeId);
    // For a hit inside a compact fold, both the fold row and the matched
    // child tool card count as current (the card auto-expands).
    if (located) set.add(match.nodeId);
    return set;
  }, [matches, selectedIdx, nodes]);

  const settle = journalStatus !== "gap-backfill";
  const defaultFolded = collapseTick > 0;
  const range = useMemo(
    () =>
      // c-perfaudit: covers the per-scrollTop rowOffsets/visibleRange
      // allocations (virtualWindow.ts) under ?profile=1.
      profileRegion("transcript.visibleRange", () =>
        visibleRange(nodes.length, sizes, scrollTop, viewport, OVERSCAN, estimate),
      ),
    [nodes.length, sizes, scrollTop, viewport, estimate],
  );

  // Bounded journal windows: the Hub serves only the newest ~2k rows on
  // attach. While the lowest LOADED seq is above 1, older history is one
  // load-earlier click away.
  const loadedFloor = useMemo(
    () => (events.length ? events.reduce((min, ev) => Math.min(min, Number(ev.seq)), Number(events[0].seq)) : 1),
    [events],
  );
  const canLoadEarlier = nodes.length > 0 && loadedFloor > 1;
  const onLoadEarlier = useCallback(async () => {
    if (!instanceId || loadingEarlier || !canLoadEarlier) return;
    const el = scrollerRef.current;
    // Hold the topmost rendered row at its viewport offset while the older
    // window expands padTop above it; the pendingScroll restore effect then
    // corrects against measured row heights as the new rows mount.
    const anchorId = nodesRef.current[range.start]?.id ?? null;
    let offset = 0;
    if (el && anchorId) {
      const row = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(anchorId)}"]`);
      if (row) offset = row.getBoundingClientRect().top - el.getBoundingClientRect().top;
    }
    pinRef.current = false;
    setLoadingEarlier(true);
    try {
      await hubStore.loadEarlier(instanceId);
      if (anchorId) {
        restoringRef.current = true;
        pendingScroll.current = { kind: "restore", anchorId, offset, tries: 0 };
        // Keep repinning as the prepended (unmeasured) rows settle; the
        // one-shot restore effect alone stops before the average converges.
        prependAnchorRef.current = { anchorId, offset, tries: 0 };
      }
    } finally {
      setLoadingEarlier(false);
    }
  }, [instanceId, loadingEarlier, canLoadEarlier, range.start]);

  // Stable per-row size reporter keyed by node id. The identity MUST stay
  // constant across parent re-renders (scroll fires setScrollTop on every
  // frame): TranscriptRow's measuring effect depends on it, so an inline
  // closure would tear down and rebuild a ResizeObserver (and force a layout
  // read) for every visible row on every scroll frame.
  const setRowSize = useCallback((id: string, height: number) => {
    if (height <= 0) return;
    // Sub-pixel tolerance: measured heights jitter by fractions of a px
    // between frames; ignore deltas under 1 instead of thrashing state.
    setRowHeights((prev) => {
      const last = prev.get(id);
      if (last !== undefined && Math.abs(last - height) < 1) return prev;
      const next = new Map(prev);
      next.set(id, height);
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

  // Load-earlier anchors (repin loop declared here so it sits by the scroll
  // math it uses): the prepended window is mostly UNMEASURED rows rendered at
  // `estimate` height. When the average later converges, padTop for thousands
  // of rows shifts after the one-shot DOM restore finished — keep re-pinning
  // the held anchor until sizes stop changing.
  useLayoutEffect(() => {
    const held = prependAnchorRef.current;
    const el = scrollerRef.current;
    if (!held || !el || !nodesRef.current.length) return;
    const rowEl = el.querySelector<HTMLElement>(`[data-anchor="${CSS.escape(held.anchorId)}"]`);
    if (!rowEl) {
      // Anchor not mounted yet (the pendingScroll restore runs after this
      // effect and brings it in): do not burn the stable-pass budget.
      return;
    }
    const delta = rowEl.getBoundingClientRect().top - el.getBoundingClientRect().top - held.offset;
    if (Math.abs(delta) > 1) {
      el.scrollTop += delta;
      scrollTopRef.current = el.scrollTop;
      held.tries = 0;
      return;
    }
    held.tries += 1;
    // Sizes/events arrive on separate commits; release after a few stable
    // passes with no correction needed.
    if (held.tries >= 4) prependAnchorRef.current = null;
  }, [sizes, nodes, estimate]);

  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    // c-mfix round 4: a height change (the soft keyboard shrinks the band)
    // does not change nodes/sizes, so the pin effect above never reruns and a
    // long transcript pinned to the tail is left scrolled above its newest
    // row. Re-pin in the same frame the scroller shrinks, but ONLY while the
    // user was already pinned to the bottom — someone who scrolled up keeps
    // their reading position when the keyboard opens.
    let wasPinned = false;
    const measure = () => {
      const next = el.clientHeight;
      const viewport = next < 32 ? 720 : next;
      viewportRef.current = viewport;
      setViewport(viewport);
      if (wasPinned) {
        el.scrollTop = el.scrollHeight;
        scrollTopRef.current = el.scrollTop;
      }
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    // ResizeObserver fires with the box AFTER the shrink; pin state must be
    // sampled from the geometry just before it, so refresh it on every scroll
    // frame and in the observer callback before `measure`.
    const ro = new ResizeObserver(() => {
      wasPinned = pinRef.current;
      measure();
    });
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

  // Explicit collapse wins over a prior reader expansion; the bumped row key
  // remounts each card with its local latch reset. Failures stay open
  // (ToolRow never passes defaultFolded to a failed card).
  const collapseAll = useCallback(() => {
    setExpandedTools(new Set());
    setCollapseTick((n) => n + 1);
  }, []);

  useImperativeHandle(handleRef, () => ({ openSearch, collapseAll }), [openSearch, collapseAll]);

  // Rows are memoised (see TranscriptRow), so the props they receive must keep
  // their identity across streaming batches. The page builds a fresh steer
  // control object and handler on every render; hold them by value / by ref.
  const steerEnabled = steerHeld?.enabled;
  const steerReason = steerHeld?.reason;
  const hasSteerHeld = steerHeld !== undefined;
  const stableSteerHeld = useMemo<SteerHeldControl | undefined>(
    () => (hasSteerHeld ? { enabled: Boolean(steerEnabled), reason: steerReason ?? "" } : undefined),
    [hasSteerHeld, steerEnabled, steerReason],
  );
  const onSteerHeldRef = useRef(onSteerHeld);
  useLayoutEffect(() => {
    onSteerHeldRef.current = onSteerHeld;
  });
  const hasSteerHandler = onSteerHeld !== undefined;
  const stableOnSteerHeld = useMemo<SteerHeldHandler | undefined>(
    () => (hasSteerHandler ? (iid, bid) => onSteerHeldRef.current?.(iid, bid) : undefined),
    [hasSteerHandler],
  );

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
      {toolbar ? (
      <div
        className={css.toolbar}
        data-testid="transcript-toolbar"
        data-tools-fold={toolsFolded ? "1" : "0"}
      >
        {toolsFolded ? (
          <button
            type="button"
            className={`${ui.chip} ${css.toolsTrigger}`}
            data-testid="transcript-tools-open"
            aria-label="transcript 操作"
            // Plain disclosure: expanding swaps this trigger out for the
            // inline chips (no menu role, no focus move, no Escape surface),
            // so there is no haspopup and expanded stays false here.
            aria-expanded={false}
            onClick={() => setToolsOpen(true)}
          >
            ⋯
          </button>
        ) : (
          <>
            <button
              type="button"
              className={ui.chip}
              data-testid="collapse-all"
              onClick={() => {
                collapseAll();
                if (compactLayout) setToolsOpen(false);
              }}
            >
              全部折叠
            </button>
            <button
              ref={openButtonRef}
              type="button"
              className={ui.chip}
              data-testid="transcript-search-open"
              aria-expanded={searchOpen}
              onClick={() => {
                if (searchOpen) closeSearch();
                else openSearch();
                if (compactLayout) setToolsOpen(false);
              }}
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
                  if (compactLayout) setToolsOpen(false);
                }}
              >
                {showInjected ? `隐藏注入内容 · ${injectedCount}` : `显示注入内容 · ${injectedCount}`}
              </button>
            ) : null}
          </>
        )}
      </div>
      ) : null}
      {searchOpen ? (
        <div
          className={css.searchbar}
          data-testid="transcript-searchbar"
          role="search"
          aria-label="正文搜索"
        >
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
          {canLoadEarlier ? (
            <div data-testid="load-earlier-wrap" className={css.loadEarlier}>
              <button
                type="button"
                className={ui.chip}
                data-testid="load-earlier"
                disabled={loadingEarlier}
                onClick={() => void onLoadEarlier()}
              >
                {loadingEarlier ? "加载中…" : "加载更早的记录"}
              </button>
            </div>
          ) : null}
          <div style={{ height: range.padTop }} aria-hidden />
          {slice.map((node) => {
            const hit = hitNodes.get(node.id);
            return (
              <TranscriptRow
                key={node.id}
                node={node}
                active={activeTurn === node.id}
                defaultFolded={defaultFolded}
                collapseTick={collapseTick}
                settle={settle}
                onSize={setRowSize}
                searchHit={Boolean(hit)}
                searchCurrent={Boolean(hit?.current)}
                instanceId={instanceId}
                // Only held rows read the steer control; scoping it keeps an
                // instance-state flip from re-committing every visible row.
                steerHeld={node.type === "message" && node.local?.held ? stableSteerHeld : undefined}
                onSteerHeld={stableOnSteerHeld}
                steering={steeringRef.current}
                expandTick={node.type === "compact" && compactHit?.compactId === node.id ? expandTick : 0}
                hitChildId={
                  node.type === "compact"
                    ? (compactHit?.childId ?? null)
                    : node.type === "tool"
                      ? (hit?.childId ?? null)
                      : null
                }
                dismissedWorkflows={dismissedWorkflows}
                onToggleWorkflowDismiss={toggleWorkflowDismiss}
                expandedTools={expandedTools}
                onToggleToolExpand={toggleToolExpand}
                currentHitIds={currentHitIds}
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

type TranscriptRowProps = {
  node: TranscriptNode;
  active: boolean;
  defaultFolded: boolean;
  collapseTick: number;
  settle: boolean;
  onSize: (id: string, height: number) => void;
  searchHit: boolean;
  searchCurrent: boolean;
  expandTick: number;
  hitChildId: string | null;
  instanceId: string;
  steerHeld?: SteerHeldControl;
  onSteerHeld?: SteerHeldHandler;
  steering: Set<string>;
  dismissedWorkflows: ReadonlySet<string>;
  onToggleWorkflowDismiss: (workflowId: string, dismiss: boolean) => void;
  expandedTools: ReadonlySet<string>;
  onToggleToolExpand: (nodeId: string, expanded: boolean) => void;
  currentHitIds: ReadonlySet<string>;
};

/**
 * Structural equality for assembled nodes. assembleTranscript rebuilds every
 * node object on each journal batch, so identity says nothing; only the
 * mounted window (a few dozen rows) is ever compared.
 */
function sameValue(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i += 1) if (!sameValue(a[i], b[i])) return false;
    return true;
  }
  if (Array.isArray(b)) return false;
  // Only plain records recurse; anything else (Date, Map, class instances)
  // compares by identity above.
  const proto = Object.getPrototypeOf(a);
  if ((proto !== Object.prototype && proto !== null) || Object.getPrototypeOf(b) !== proto) return false;
  const ak = Object.keys(a);
  const bk = Object.keys(b);
  if (ak.length !== bk.length) return false;
  for (const k of ak) {
    if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
    if (!sameValue((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k])) return false;
  }
  return true;
}

function sameSet(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
  if (a === b) return true;
  if (a.size !== b.size) return false;
  for (const v of a) if (!b.has(v)) return false;
  return true;
}

/**
 * A streaming batch must commit only the row whose content changed. The
 * parent rebuilds nodes and derived sets per batch, so compare by value; the
 * callbacks are stable by construction (see TranscriptInner).
 */
function sameRowProps(prev: TranscriptRowProps, next: TranscriptRowProps): boolean {
  return (
    prev.active === next.active
    && prev.defaultFolded === next.defaultFolded
    && prev.collapseTick === next.collapseTick
    && prev.settle === next.settle
    && prev.onSize === next.onSize
    && prev.searchHit === next.searchHit
    && prev.searchCurrent === next.searchCurrent
    && prev.expandTick === next.expandTick
    && prev.hitChildId === next.hitChildId
    && prev.instanceId === next.instanceId
    && prev.steerHeld === next.steerHeld
    && prev.onSteerHeld === next.onSteerHeld
    && prev.steering === next.steering
    && prev.onToggleWorkflowDismiss === next.onToggleWorkflowDismiss
    && prev.onToggleToolExpand === next.onToggleToolExpand
    && sameSet(prev.dismissedWorkflows, next.dismissedWorkflows)
    && sameSet(prev.expandedTools, next.expandedTools)
    && sameSet(prev.currentHitIds, next.currentHitIds)
    && sameValue(prev.node, next.node)
  );
}

// c-perfaudit probe (?profile=1 only): one `commit:TranscriptRow` sample per
// row that actually re-rendered in a commit, tagged with its node id.
function rowCommitProbe(nodeId: string): ProfilerOnRenderCallback {
  return (_id, phase, actualDuration) => {
    reportProbe("commit:TranscriptRow", { phase, actualDuration, nodeId });
  };
}

const TranscriptRow = memo(function TranscriptRow({
  node,
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
  dismissedWorkflows,
  onToggleWorkflowDismiss,
  expandedTools,
  onToggleToolExpand,
  currentHitIds,
}: TranscriptRowProps) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    // The row's own padding carries the gap to the next row (no margins, and
    // display: flow-root keeps child margins inside), so the border box IS
    // the slot the virtual window must reserve.
    const report = () => onSize(node.id, el.getBoundingClientRect().height);
    report();
    if (typeof ResizeObserver === "undefined") return;
    // Also re-measures asynchronous growth (syntax highlight, lazy math,
    // images); the scroller keeps the reading anchor across it.
    const ro = new ResizeObserver(report);
    ro.observe(el);
    return () => ro.disconnect();
    // Depend only on the stable id (and the stable onSize callback): fold /
    // content changes resize the element, and this ResizeObserver fires for
    // those automatically. Depending on the `node` object identity would
    // rebuild the observer on every parent scroll re-render, since the
    // assembled node list is re-created each render.
  }, [onSize, node.id]);
  const probe = useMemo(() => (profilingEnabled ? rowCommitProbe(node.id) : null), [node.id]);
  const content = renderNode(node, {
    defaultFolded,
    collapseTick,
    settle,
    expandTick,
    hitChildId,
    instanceId,
    steerHeld,
    onSteerHeld,
    steering,
    dismissedWorkflows,
    onToggleWorkflowDismiss,
    expandedTools,
    onToggleToolExpand,
    searchCurrent,
    currentHitIds,
  });
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
      {probe ? (
        <Profiler id="TranscriptRow" onRender={probe}>
          {content}
        </Profiler>
      ) : (
        content
      )}
    </div>
  );
}, sameRowProps);

/** A failed tool gets a visible inline tag and never obeys collapse-all. */
function ToolRow({
  node,
  opts,
  hitChildId,
}: {
  node: Extract<TranscriptNode, { type: "tool" }>;
  opts: {
    defaultFolded: boolean;
    collapseTick: number;
    settle: boolean;
    dismissedWorkflows: ReadonlySet<string>;
    onToggleWorkflowDismiss: (workflowId: string, dismiss: boolean) => void;
    expandedTools: ReadonlySet<string>;
    onToggleToolExpand: (nodeId: string, expanded: boolean) => void;
    searchCurrent: boolean;
  };
  hitChildId?: string | null;
}): ReactNode {
  const failed = isToolFailure(node);
  const workflowId = node.workflow?.run.workflowId;
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
      foldSettled
      settle={opts.settle}
      // A current in-transcript search hit inside this card must not stay
      // hidden behind the D-041 fold (same auto-open the transcript gives
      // CompactFold/SubagentFolds for a hit).
      expanded={opts.expandedTools.has(node.id) || opts.searchCurrent}
      onExpand={() => opts.onToggleToolExpand(node.id, true)}
      workflowDismissed={workflowId ? opts.dismissedWorkflows.has(workflowId) : false}
      onDismissWorkflow={workflowId ? () => opts.onToggleWorkflowDismiss(workflowId, true) : undefined}
      onUndismissWorkflow={workflowId ? () => opts.onToggleWorkflowDismiss(workflowId, false) : undefined}
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
    dismissedWorkflows: ReadonlySet<string>;
    onToggleWorkflowDismiss: (workflowId: string, dismiss: boolean) => void;
    expandedTools: ReadonlySet<string>;
    onToggleToolExpand: (nodeId: string, expanded: boolean) => void;
    searchCurrent: boolean;
    /** Node ids carrying the CURRENT search hit (compact-fold children). */
    currentHitIds?: ReadonlySet<string> | null;
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
        // t-annotations: the message is an in-message ① anchor surface; the
        // role/status header sits in the same section but the selection
        // helper in the specs targets the prose element directly. Owning
        // session/readonly state come from the session-page ancestor.
        data-anchor-surface="transcript"
        data-anchor-message={node.id}
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
          // The stored text is untouched; only the streaming render closes a
          // dangling ``` so the partial block does not flip prose/code.
          <MarkdownText text={streaming ? balanceFences(node.text) : node.text} />
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
        {streaming ? <StreamingCursor text={node.text} /> : null}
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
  }  if (node.type === "workflow") {
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
              opts={{
                defaultFolded: opts.defaultFolded,
                collapseTick: opts.collapseTick,
                settle: opts.settle,
                dismissedWorkflows: opts.dismissedWorkflows,
                onToggleWorkflowDismiss: opts.onToggleWorkflowDismiss,
                expandedTools: opts.expandedTools,
                onToggleToolExpand: opts.onToggleToolExpand,
                // Compact-fold children take their hit status from THIS
                // child's own always-current hit map — not from the
                // CompactFold latch (which is not re-set when the selected
                // hit moves between children of an already-open fold).
                searchCurrent: Boolean(opts.currentHitIds?.has(child.id)),
              }}
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
    // c-mfix: effort/model are KNOWN observation kinds (protocol §5.1); only
    // this dispatch site changed — assembly/windowing logic is untouched. The
    // opaque fallback node carries kind="effort"|"model" plus the raw payload.
    if (node.kind === "model" || node.kind === "effort") {
      return <ObservedChangeRow kind={node.kind} raw={node.raw} />;
    }
    return <OpaqueRow kind={node.kind} summary={node.summary} raw={node.raw} />;
  }
  return null;
}

/** Last non-blank text node inside `root`, in document order. */
function lastTextNode(root: Element): Text | null {
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let last: Text | null = null;
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if ((n.nodeValue ?? "").trim()) last = n as Text;
  }
  return last;
}

/**
 * The streaming caret. It is absolutely positioned at the end of the last
 * rendered glyph, so it takes no line box of its own: the row keeps the same
 * height when streaming starts and ends. Its box is clamped to what is
 * visible: inside every clipping ancestor of the glyph (a code block's
 * horizontal scroller) and inside the section's content box, so a long
 * unwrapped line can never push it out and widen any scroll extent.
 */
function StreamingCursor({ text }: { text: string }) {
  const ref = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const cursor = ref.current;
    const section = cursor?.parentElement;
    if (!cursor || !section) return;
    let textNode: Text | null = null;
    // The body is everything between the author line and the cursor.
    for (let el = cursor.previousElementSibling; el && el !== section.firstElementChild; el = el.previousElementSibling) {
      textNode = lastTextNode(el);
      if (textNode) break;
    }
    // Ancestors between the glyph and the section that clip horizontally.
    const clips: HTMLElement[] = [];
    for (let el = textNode?.parentElement ?? null; el && el !== section; el = el.parentElement) {
      if (getComputedStyle(el).overflowX !== "visible") clips.push(el);
    }
    const place = () => {
      const at = textNode ? caretRect(textNode) : null;
      if (!at) {
        cursor.style.left = "";
        cursor.style.top = "";
        return;
      }
      const base = section.getBoundingClientRect();
      const pad = getComputedStyle(section);
      let minX = base.left + section.clientLeft + parseFloat(pad.paddingLeft);
      let maxX = base.left + section.clientLeft + section.clientWidth - parseFloat(pad.paddingRight);
      for (const clip of clips) {
        const box = clip.getBoundingClientRect();
        minX = Math.max(minX, box.left + clip.clientLeft);
        maxX = Math.min(maxX, box.left + clip.clientLeft + clip.clientWidth);
      }
      const width = cursor.offsetWidth;
      const x = Math.max(minX, Math.min(at.right + 1, maxX - width));
      cursor.style.left = `${x - base.left - section.clientLeft}px`;
      cursor.style.top = `${at.top - base.top - section.clientTop + Math.max(0, (at.height - cursor.offsetHeight) / 2)}px`;
    };
    place();
    for (const clip of clips) clip.addEventListener("scroll", place, { passive: true });
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(place);
    ro?.observe(section);
    return () => {
      for (const clip of clips) clip.removeEventListener("scroll", place);
      ro?.disconnect();
    };
  }, [text]);
  return <span ref={ref} className={css.cursor} data-testid="streaming-cursor" aria-hidden />;
}

/** Viewport rect of the last visible glyph (a whole code point) in `node`. */
function caretRect(node: Text): DOMRect | null {
  const value = node.nodeValue ?? "";
  const end = value.trimEnd().length;
  if (end === 0) return null;
  let start = end - 1;
  // Step back over a surrogate pair so an emoji measures its full glyph.
  if (start > 0 && /[\uDC00-\uDFFF]/.test(value[start]) && /[\uD800-\uDBFF]/.test(value[start - 1])) start -= 1;
  const range = node.ownerDocument.createRange();
  range.setStart(node, start);
  range.setEnd(node, end);
  // jsdom has no layout: getClientRects is absent there.
  const rects = typeof range.getClientRects === "function" ? range.getClientRects() : null;
  const rect = rects && rects.length ? rects[rects.length - 1] : null;
  return rect && rect.height > 0 ? rect : null;
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
      {open ? <div className={css.foldBody}>{children}</div> : null}
    </div>
  );
}
