import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Observation } from "../../types/observation";
import { MarkdownText } from "../../components/MarkdownText";
import type { LocalBubble } from "../../lib/store";
import { SentAttachments } from "./AttachmentChips";
import { hubStore } from "../../lib/store";
import ui from "../../styles/ui.module.css";
import { assembleTranscript, compactTranscript, type TranscriptNode } from "./assemble";
import { JournalBanner, type JournalUiStatus } from "./JournalBanner";
import { ToolCard } from "./ToolCard";
import { WorkflowTree } from "./WorkflowTree";
import { UsageFooter } from "./UsageFooter";
import { OpaqueRow } from "./OpaqueRow";
import css from "./Transcript.module.css";
import session from "./session.module.css";
import { DEFAULT_ROW, OVERSCAN, visibleRange } from "./virtualWindow";
import { readShowInjected, writeShowInjected } from "./injectedPref";
import type { MessageOrigin } from "../../types/generated";

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

export function Transcript({
  events,
  bubbles = [],
  compact = true,
  journalStatus = "live",
  onRetryJournal,
}: {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
  journalStatus?: JournalUiStatus;
  onRetryJournal?: () => void;
}) {
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
  const pinRef = useRef(true);
  const nodesRef = useRef(nodes);
  const sizesHold = useRef(sizes);
  useEffect(() => {
    sizesHold.current = sizes;
  }, [sizes]);

  const settle = journalStatus !== "gap-backfill";
  const defaultFolded = collapseTick > 0;
  const range = useMemo(
    () => visibleRange(nodes.length, sizes, scrollTop, viewport, OVERSCAN, DEFAULT_ROW),
    [nodes.length, sizes, scrollTop, viewport],
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

  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const measure = () => {
      const next = el.clientHeight;
      setViewport(next < 32 ? 720 : next);
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

  useLayoutEffect(() => {
    const el = scrollerRef.current;
    if (!el || !pinRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [nodes.length, sizes]);

  const scrollToIndex = useCallback((index: number) => {
    const el = scrollerRef.current;
    if (!el || index < 0) return;
    pinRef.current = index >= nodesRef.current.length - 1;
    let acc = 0;
    const current = sizesHold.current;
    for (let i = 0; i < index; i++) {
      const sz = current[i];
      acc += sz > 0 ? sz : DEFAULT_ROW;
    }
    el.scrollTop = acc;
  }, []);

  const turnIds = useMemo(
    () => nodes.filter((n) => n.type === "message").map((n) => n.id),
    [nodes],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.closest("textarea, input, select, [contenteditable='true']")) return;
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
  }, [activeTurn, turnIds, scrollToIndex]);

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
      <div
        ref={scrollerRef}
        className={css.scroller}
        data-testid="transcript-scroller"
        onScroll={(event) => {
          const el = event.currentTarget;
          setScrollTop(el.scrollTop);
          pinRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 64;
        }}
      >
        <div className={css.list}>
          <div style={{ height: range.padTop }} aria-hidden />
          {slice.map((node, i) => {
            const index = range.start + i;
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
}: {
  node: TranscriptNode;
  index: number;
  active: boolean;
  defaultFolded: boolean;
  collapseTick: number;
  settle: boolean;
  onSize: (index: number, height: number) => void;
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
  }, [index, onSize, node, collapseTick]);
  return (
    <div
      ref={ref}
      id={`t-${node.id}`}
      data-testid="transcript-row"
      data-anchor={node.id}
      data-kind={node.type}
      data-turn-active={active ? "1" : "0"}
      className={active ? `${css.row} ${css.rowActive}` : css.row}
    >
      {renderNode(node, { defaultFolded, collapseTick, settle })}
    </div>
  );
}

function renderNode(
  node: TranscriptNode,
  opts: { defaultFolded: boolean; collapseTick: number; settle: boolean },
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
    return (
      <section
        className={user ? session.user : session.assistant}
        data-testid={node.local ? "optimistic-bubble" : "message"}
        data-status={node.status}
      >
        <div className={session.you}>
          {user ? "You" : node.role}
          {node.local ? ` · ${node.local.state}` : ""}
          {/* Status order the composer and transcript share: a queued message
              has not been sent, a streaming one is still arriving. */}
          {node.status === "queued" ? <span className={session.stat}> · 排队中</span> : null}
          {node.status === "interrupted" ? <span className={session.stat}> · 已打断</span> : null}
        </div>
        {node.role === "assistant" ? <MarkdownText text={node.text} /> : <p className={session.bubble}>{node.text}</p>}
        {streaming ? <span className={session.cursor} data-testid="streaming-cursor" aria-hidden /> : null}
        {node.local?.attachments?.length ? (
          <SentAttachments attachments={node.local.attachments} />
        ) : null}
        {node.local?.state === "queued" ? (
          <button className={ui.chip} onClick={() => hubStore.retract(node.local!.id)}>
            撤回
          </button>
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
    return (
      <ToolCard
        key={`${node.id}:${opts.collapseTick}`}
        driverKind={node.driverKind}
        call={node.call}
        result={node.result}
        completeness={node.completeness}
        diffState={node.diffState}
        defaultFolded={opts.defaultFolded}
        settle={opts.settle}
      />
    );
  }
  if (node.type === "workflow") {
    return <WorkflowTree run={node.run} phases={node.phases} members={node.members} />;
  }
  if (node.type === "usage") {
    return <UsageFooter payload={node.payload} />;
  }
  if (node.type === "compact") {
    return (
      <CompactFold toolCount={node.toolCount} thoughtCount={node.thoughtCount}>
        {node.children.map((child) =>
          child.type === "tool" ? (
            <ToolCard
              key={`${child.id}:${opts.collapseTick}`}
              driverKind={child.driverKind}
              call={child.call}
              result={child.result}
              completeness={child.completeness}
              diffState={child.diffState}
              defaultFolded={opts.defaultFolded}
              settle={opts.settle}
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
  children,
}: {
  toolCount: number;
  thoughtCount: number;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button className={session.fold} data-testid="compact-fold" onClick={() => setOpen(!open)}>
        <span>▸</span>
        <span>{open ? "收起过程" : `${toolCount} 次工具 · ${thoughtCount} 段思考`}</span>
      </button>
      {open ? <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>{children}</div> : null}
    </div>
  );
}
