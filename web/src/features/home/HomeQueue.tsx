import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { hubStore, useHub, useMainEntry } from "../../lib/store";
import { thisDeviceId } from "../../lib/interactionStatus";
import { formatListTime } from "../../lib/format";
import { useHeldOrder, type HoldSource } from "../../lib/useHeldOrder";
import type { Id } from "../../types/wire";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import { DecisionCard, type DecisionView, type HomeCardState } from "../approvals/ApprovalCard";
import { useInboxClockNow } from "../mobile/useInboxPendingCount";
import { syncInboxDeadlineClock } from "../mobile/inboxClock";
import { useTaskLedger } from "../tasks/taskLedger";
import { displayKeysByTaskId } from "../tasks/boardColumns";
import { HOME_FRESHNESS, useHomeFreshness } from "./freshness";
import { formatGlanceCount, glanceCounts, type GlanceSource } from "./glance";
import { GLANCE_TARGETS } from "./glanceNav";
import {
  HOME_QUEUE_LIMIT,
  HOME_STALE_TEXT,
  answeredElsewhere,
  deriveHomeQueue,
  inboxHref,
  type HomeQueueCard,
  type Receipt,
} from "./homeQueue";
import { noticeStore, noticeVoice, useOwnerNotices } from "./notices";
import { HUB_HAND_OFF_STORE, handToMain, handToMainText, reconcileHandOff, type HandOffOutcome } from "./handToMain";
import css from "./homeQueue.module.css";

/**
 * A card answered from here keeps its slot from the tap on: `sending` while
 * the POST is in flight (the store already flips the row to committed before
 * the call returns, so the live list drops it early), then its §4.4 receipt.
 */
type ReceiptEntry = { receipt: Receipt | "sending"; card: HomeQueueCard; index: number };

/**
 * ④ 「需要你」 (home.md §4): the page's ONE needs-you queue, on `/m` (phone)
 * and `/sessions` (desktop). Interaction cards are `DecisionCard
 * mode="home"`; 「任务待你」, 「告诉你」 and 回合失败 cards are drawn here. The header
 * number is `glanceCounts()` — the same N as ① and the OS badge (§5.1).
 *
 * - collapsed: 3 cards on the phone, a 2×4 grid on desktop; 「还有 N 条 ▾」
 *   expands in place (not persisted; 「收起 ▴」 closes it). §3.3 holds the
 *   order only while the reader interacts — a press, focus on a row control
 *   or an open sheet — never merely because the expansion is open;
 * - hidden while the ledger search has text;
 * - `?focus=<id>` (push landing, §9) expands if needed, scrolls the card
 *   into view and rings it.
 */
export function HomeQueue({
  placement,
  searching,
}: {
  placement: "phone" | "desktop";
  searching: boolean;
}) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  const { tasks, projectName } = useTaskLedger(null);
  const deviceId = useMemo(() => thisDeviceId(), []);
  const nowMs = useInboxClockNow();
  useEffect(() => {
    syncInboxDeadlineClock(hub.interactions);
  }, [hub.interactions]);
  const [params] = useSearchParams();
  const focus = params.get("focus");

  const source = useMemo<GlanceSource>(
    () => ({
      instances: hub.instances,
      interactions: hub.interactions,
      hosts: hub.hosts,
      tasks,
      answering: hub.answering,
      deviceId,
      nowMs,
    }),
    [hub.instances, hub.interactions, hub.hosts, tasks, hub.answering, deviceId, nowMs],
  );
  // §4.1 kind 4: the unacknowledged owner notices (not counted in N).
  const { notices } = useOwnerNotices();
  // §4.1 item 1: Main's own pending interactions are pinned, labelled and
  // counted once — they are the same queue rows, just moved to the front.
  const main = useMainEntry();
  const queue = useMemo(() => deriveHomeQueue(source, notices, main), [source, notices, main]);
  // §6.4: a QUESTION or PLAN card may quote itself to Main. The quote never
  // answers the interaction, so the card stays pending until a real answer.
  // The outcome is the command's SETTLED state, not the outbox enqueue: a
  // paused/fenced Main must never read as 「已交给 Main」 (§6.4 r2).
  const handMain = useCallback(
    async (item: Interaction): Promise<HandOffOutcome> => {
      if (!main) return { kind: "rejected", reason: null, commandId: null };
      const text = handToMainText({
        item,
        sessionTitle: hubStore.titleOf(item.instanceId),
        hostLabel: hubStore.hostName(item.hostId),
      });
      return handToMain(HUB_HAND_OFF_STORE, main.instance.id, text);
    },
    [main],
  );
  const reconcileHand = useCallback(
    (commandId: string) => reconcileHandOff(HUB_HAND_OFF_STORE, commandId),
    [],
  );
  // §5.1: the header N is the count row's N, read from the one derivation.
  const needYou = useMemo(() => glanceCounts(source).needYou, [source]);

  // §4.4 receipts: a card answered here keeps its slot for 2 s with its
  // receipt, then leaves — even if a poll already dropped it, and even if a
  // late poll still lists it pending (`gone` hides it; ids are never reused).
  // A receipt carries the card's last painted content and slot.
  const [receipts, setReceipts] = useState<ReadonlyMap<string, ReceiptEntry>>(new Map());
  const [gone, setGone] = useState<ReadonlySet<string>>(new Set());
  const timers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  // What the last commit painted; read only from handlers (settle, holds).
  const paintedRef = useRef<readonly HomeQueueCard[]>([]);
  useEffect(
    () => () => {
      for (const timer of timers.current.values()) clearTimeout(timer);
      timers.current.clear();
    },
    [],
  );
  const settle = useCallback((id: string, receipt: Receipt) => {
    setReceipts((current) => {
      const entry = current.get(id);
      return entry ? new Map(current).set(id, { ...entry, receipt }) : current;
    });
    const previous = timers.current.get(id);
    if (previous) clearTimeout(previous);
    timers.current.set(
      id,
      setTimeout(() => {
        timers.current.delete(id);
        setGone((current) => new Set(current).add(id));
        setReceipts((current) => {
          const next = new Map(current);
          next.delete(id);
          return next;
        });
      }, HOME_FRESHNESS.receiptMs),
    );
  }, []);
  const respond = useCallback(
    async (item: Interaction, answer: InteractionAnswer) => {
      // Pin the card's painted content and slot BEFORE the POST: the store
      // marks the row committed before respond() resolves.
      const painted = paintedRef.current;
      const index = painted.findIndex((card) => card.id === item.id);
      if (index >= 0) {
        const pinned = painted[index];
        setReceipts((current) => new Map(current).set(item.id, { receipt: "sending", card: pinned, index }));
      }
      try {
        await hubStore.respond(item.id, answer);
      } catch (error) {
        if (!answeredElsewhere(error)) {
          setReceipts((current) => {
            if (current.get(item.id)?.receipt !== "sending") return current;
            const next = new Map(current);
            next.delete(item.id);
            return next;
          });
          throw error;
        }
        settle(item.id, "elsewhere");
        // The queue still lists it until the next list; fetch it now.
        void hubStore.refresh().catch(() => undefined);
        return;
      }
      settle(item.id, "submitted");
    },
    [settle],
  );

  const live = useMemo(
    () => queue.cards.filter((card) => !gone.has(card.id)),
    [queue.cards, gone],
  );

  // §3.3: ONLY an interaction holds the order — a press, focus on a row
  // control or an open sheet/menu (§4.1's 「展开期间同样遵守 §3.3」 is the same
  // rule applied inside the expansion, never a hold of its own). The snapshot
  // is the painted card list; while held, live content replaces each card by
  // id, a card that left stays as an inert placeholder (unless it is showing
  // its receipt) and a new card waits for the release — which the hook's
  // pointerup/blur/idle paths always provide.
  const { begin, end, held, snapshot, regionRef } = useHeldOrder<readonly HomeQueueCard[]>();
  const startHold = useCallback(
    (source: HoldSource) => begin(source, () => paintedRef.current),
    [begin],
  );

  const painted = useMemo<readonly HomeQueueCard[]>(() => {
    const liveById = new Map(live.map((card) => [card.id, card]));
    if (snapshot) {
      return snapshot
        .filter((card) => !gone.has(card.id))
        .map((card) => liveById.get(card.id) ?? pinnedCard(receipts.get(card.id)) ?? card);
    }
    // A receipt card the live data already dropped keeps its painted slot.
    const out = [...live];
    for (const [id, entry] of [...receipts].sort((a, b) => a[1].index - b[1].index)) {
      if (!liveById.has(id)) out.splice(Math.min(entry.index, out.length), 0, pinnedCard(entry)!);
    }
    return out;
  }, [live, snapshot, gone, receipts]);
  useEffect(() => {
    paintedRef.current = painted;
  }, [painted]);
  const liveIds = useMemo(() => new Set(live.map((card) => card.id)), [live]);
  const ids = useMemo(() => painted.map((card) => card.id), [painted]);

  const limit = HOME_QUEUE_LIMIT[placement];
  // Not persisted (§4.1): null = the default, which is collapsed unless a
  // ?focus card sits behind the fold; a tap on the fold row decides from then on.
  const [expandedChoice, setExpanded] = useState<boolean | null>(null);
  const focusIndex = focus ? ids.indexOf(focus) : -1;
  const expanded = expandedChoice ?? focusIndex >= limit;
  // A hold must never strand the queue: if the held cards are all answered
  // (or otherwise leave), the painted list empties while a source is still
  // open. Release everything — the queue goes live again and the empty branch
  // cannot hide new arrivals behind a frozen, empty snapshot.
  useEffect(() => {
    if (!held || painted.length > 0) return;
    end("pointer");
    end("focus");
    end("sheet");
  }, [held, painted.length, end]);

  // Push landing: scroll the focused card into view once per focus value.
  const scrolledFor = useRef<string | null>(null);
  const focusVisible = focusIndex >= 0 && (expanded || focusIndex < limit);
  useEffect(() => {
    if (!focus || !focusVisible || scrolledFor.current === focus) return;
    const el = regionRef.current?.querySelector<HTMLElement>(`[data-card-id="${CSS.escape(focus)}"]`);
    if (!el) return;
    scrolledFor.current = focus;
    el.scrollIntoView({ block: "center", behavior: "auto" });
  }, [focus, focusVisible, regionRef]);

  if (searching) return null;
  const stale = freshness.state === "stale";
  const firstSnapshot = freshness.state === "unknown";
  const anchor = GLANCE_TARGETS.queueId;

  if (firstSnapshot) {
    return (
      <section className={css.queue} data-testid="home-queue" data-placement={placement} id={anchor} aria-busy="true">
        <h2 className={css.queueTitle}>需要你 · –</h2>
        <div className={css.grid}>
          <div className={css.skeleton} data-testid="home-queue-skeleton" />
          <div className={css.skeleton} data-testid="home-queue-skeleton" />
        </div>
      </section>
    );
  }

  if (ids.length === 0) {
    // The header number is the LIVE §5.1 count, never the painted slice: a
    // held snapshot that has emptied must not claim zero while ① says N.
    return (
      <section
        className={css.queue}
        data-testid="home-queue"
        data-placement={placement}
        data-empty="1"
        data-order-held={held ? "1" : "0"}
        id={anchor}
      >
        <h2 className={css.queueTitle} data-testid="home-queue-title">
          需要你 · {formatGlanceCount(needYou)}
        </h2>
      </section>
    );
  }

  const visible = expanded ? painted : painted.slice(0, limit);
  const hidden = painted.length - visible.length;

  return (
    <section
      className={css.queue}
      data-testid="home-queue"
      data-placement={placement}
      data-stale={stale ? "1" : undefined}
      data-order-held={held ? "1" : "0"}
      id={anchor}
    >
      <header className={css.queueHead}>
        <h2 className={css.queueTitle} data-testid="home-queue-title">
          需要你 · {formatGlanceCount(needYou)}
        </h2>
        {queue.earliestInteractionId ? (
          <Link
            className={css.inboxLink}
            to={inboxHref(placement, queue.earliestInteractionId)}
            data-testid="home-queue-inbox"
          >
            去收件箱 ›
          </Link>
        ) : null}
      </header>
      <div
        className={css.grid}
        ref={regionRef}
        onPointerDown={() => startHold("pointer")}
        onFocus={() => startHold("focus")}
        onBlur={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget as Node | null)) end("focus");
        }}
      >
        {visible.map((card) => {
          const id = card.id;
          const inert = !liveIds.has(id) && !receipts.has(id);
          return (
            <QueueCard
              key={id}
              card={card}
              placement={placement}
              focused={focus === id}
              receipt={receiptOf(receipts.get(id))}
              stale={stale}
              inert={inert}
              screenLines={card.type === "interaction" ? hub.screens[card.entry.item.instanceId]?.lines : undefined}
              workspaceLabel={
                card.type === "interaction"
                  ? workspaceLabelOf(hub.workspaces, card.entry.instance?.workspaceId)
                  : card.type === "failed"
                    ? workspaceLabelOf(hub.workspaces, card.instance.workspaceId)
                    : ""
              }
              displayKey={card.type === "task" ? displayKeyOf(tasks, card.task.id) : null}
              projectLabel={card.type === "task" ? projectName(card.task.projectId) ?? null : null}
              mainAsking={card.type === "interaction" ? card.mainAsking === true : false}
              // §6.4: never Main's own question back to Main, and only while
              // Main is live (a paused/ended chapter would never receive it).
              handToMain={
                main && main.live && !(card.type === "interaction" && card.mainAsking)
                  ? handMain
                  : null
              }
              reconcileHandOff={main ? reconcileHand : null}
              nowMs={nowMs}
              onRespond={respond}
            />
          );
        })}
      </div>
      {expanded ? (
        // Always offered while expanded, even after cards leave and nothing
        // is hidden any more: 「收起 ▴」 just closes the expansion (it is not a
        // hold — §3.3 holds only on a press, focus or an open sheet).
        <button
          type="button"
          className={css.more}
          data-testid="home-queue-more"
          aria-expanded
          onClick={() => setExpanded(false)}
        >
          收起 ▴
        </button>
      ) : hidden > 0 ? (
        <button
          type="button"
          className={css.more}
          data-testid="home-queue-more"
          aria-expanded={false}
          onClick={() => setExpanded(true)}
        >
          还有 {hidden} 条 ▾
        </button>
      ) : null}
    </section>
  );
}

/** The receipt a card shows (none while its POST is in flight). */
function receiptOf(entry: ReceiptEntry | undefined): Receipt | null {
  return entry && entry.receipt !== "sending" ? entry.receipt : null;
}

/**
 * The pinned content of a card the live list already dropped. While its POST
 * is in flight an interaction card reads `answering`: buttons stay disabled
 * (§4.4, no optimistic update) until the receipt replaces them.
 */
function pinnedCard(entry: ReceiptEntry | undefined): HomeQueueCard | undefined {
  if (!entry) return undefined;
  const { card } = entry;
  if (card.type !== "interaction") return card;
  return { ...card, entry: { ...card.entry, uiState: "answering" } };
}

function workspaceLabelOf(
  workspaces: ReturnType<typeof hubStore.getSnapshot>["workspaces"],
  workspaceId: string | null | undefined,
): string {
  if (!workspaceId) return "";
  return workspaces.find((workspace) => workspace.id === workspaceId)?.label ?? "";
}

let keysCache: { tasks: unknown; keys: Map<string, string> } | null = null;

/** SE-nn per project, cached per ledger snapshot (it only changes on a poll). */
function displayKeyOf(tasks: Parameters<typeof displayKeysByTaskId>[0], taskId: string): string | null {
  if (!keysCache || keysCache.tasks !== tasks) keysCache = { tasks, keys: displayKeysByTaskId(tasks) };
  return keysCache.keys.get(taskId) ?? null;
}

function QueueCard({
  card,
  placement,
  focused,
  receipt,
  stale,
  inert,
  screenLines,
  workspaceLabel,
  displayKey,
  projectLabel,
  mainAsking,
  handToMain,
  reconcileHandOff,
  nowMs,
  onRespond,
}: {
  card: HomeQueueCard;
  placement: "phone" | "desktop";
  focused: boolean;
  receipt: Receipt | null;
  stale: boolean;
  inert: boolean;
  screenLines: readonly string[] | undefined;
  workspaceLabel: string;
  displayKey: string | null;
  projectLabel: string | null;
  /** This interaction is Main's own pending question on its live chapter. */
  mainAsking: boolean;
  /** §6.4 交给 Main; null when no live Main resolves or this is its own card. */
  handToMain: ((item: Interaction) => Promise<HandOffOutcome>) | null;
  /** 核对 for an unknown hand-off command. */
  reconcileHandOff: ((commandId: string) => Promise<HandOffOutcome>) | null;
  nowMs: number;
  onRespond: (item: Interaction, answer: InteractionAnswer) => Promise<void>;
}) {
  if (card.type === "interaction") {
    const { item, instance, uiState } = card.entry;
    const hostLabel = hubStore.hostName(item.hostId);
    const view: DecisionView = {
      key: item.id,
      sig: JSON.stringify([
        item.state,
        item.kind,
        item.carrier,
        item.answerable,
        item.updatedAt,
        item.deadline,
        item.request,
        uiState,
        focused ? 1 : 0,
        hostLabel,
        workspaceLabel,
        mainAsking ? 1 : 0,
      ]),
      item,
      uiState,
      focused,
      timeLabel: formatListTime(item.createdAt, nowMs),
      hostLabel,
      workspaceLabel,
      instanceKind: instance?.kind ?? "—",
    };
    const home: HomeCardState = {
      placement,
      receipt,
      stale,
      inert,
      screenLines: screenLines ?? [],
      mainAsking,
      handToMain,
      reconcileHandOff,
    };
    return <DecisionCard view={view} mode="home" onRespond={onRespond} home={home} />;
  }
  const linkProps = inert
    ? { "aria-disabled": true, tabIndex: -1, onClick: (event: { preventDefault: () => void }) => event.preventDefault() }
    : {};
  if (card.type === "task") {
    const { task, primarySessionId } = card;
    const href = primarySessionId
      ? `/s/${primarySessionId}`
      : `/board?project=${encodeURIComponent(task.projectId)}`;
    const label = displayKey ? `任务待你 · ${displayKey}` : "任务待你";
    return (
      <article
        className={`${css.card} ${focused ? css.cardFocus : ""}`}
        data-testid="home-queue-card"
        data-card-type="task"
        data-card-id={task.id}
        data-task-id={task.id}
        data-focus={focused ? "true" : "false"}
        data-stale={inert ? "1" : undefined}
      >
        <header className={css.head}>
          <span className={css.label}>{label}</span>
          {projectLabel ? (
            <span className={css.where} title={projectLabel}>
              {projectLabel}
            </span>
          ) : null}
        </header>
        <p className={css.body} title={task.title}>
          {task.blockedReason?.trim()}
        </p>
        <div className={css.actions}>
          <Link to={href} className={css.chipLink} data-testid="home-card-task" {...linkProps}>
            打开任务
          </Link>
        </div>
      </article>
    );
  }
  if (card.type === "notice") {
    return (
      <NoticeCard card={card} focused={focused} inert={inert} stale={stale} nowMs={nowMs} linkProps={linkProps} />
    );
  }
  return (
    <FailedCard
      card={card}
      focused={focused}
      inert={inert}
      stale={stale}
      workspaceLabel={workspaceLabel}
      linkProps={linkProps}
    />
  );
}

/**
 * 回合失败 (§4.2, §4.5 after seen-hub): the error verbatim on one line,
 * 「打开」 and 「知道了」. 知道了 reports the owner's seen position, so the card
 * leaves on every device; it is not counted in N.
 */
function FailedCard({
  card,
  focused,
  inert,
  stale,
  workspaceLabel,
  linkProps,
}: {
  card: Extract<HomeQueueCard, { type: "failed" }>;
  focused: boolean;
  inert: boolean;
  /** §8: the queue is stale — 知道了 greys out like every other home action. */
  stale: boolean;
  workspaceLabel: string;
  linkProps: object;
}) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { instance, text } = card;
  const where = [hubStore.hostName(instance.hostId), workspaceLabel].filter(Boolean).join(" › ");
  return (
    <article
      className={`${css.card} ${focused ? css.cardFocus : ""}`}
      data-testid="home-queue-card"
      data-card-type="failed"
      data-card-id={instance.id}
      data-instance-id={instance.id}
      data-focus={focused ? "true" : "false"}
      data-stale={inert ? "1" : undefined}
    >
      <header className={css.head}>
        <span className={css.label}>回合失败 · {instance.kind}</span>
        {where ? (
          <span className={css.where} title={where}>
            {where}
          </span>
        ) : null}
      </header>
      <p className={css.bodyLine} title={text} data-testid="home-card-body">
        {text}
      </p>
      <div className={css.actions}>
        <Link to={`/s/${instance.id}`} className={css.chipLink} data-testid="home-card-open" {...linkProps}>
          打开
        </Link>
        <button
          type="button"
          className={css.chip}
          data-testid="home-card-ack"
          disabled={inert || busy || stale}
          onClick={() => {
            setBusy(true);
            setError(null);
            hubStore.markSeen(instance.id as Id).then(
              () => setBusy(false),
              (reason: unknown) => {
                setBusy(false);
                setError(reason instanceof Error ? reason.message : String(reason));
              },
            );
          }}
        >
          知道了
        </button>
        {stale ? (
          <span className={css.hint} data-testid="home-card-stale">
            {HOME_STALE_TEXT}
          </span>
        ) : null}
      </div>
      {error ? (
        <p className={css.hint} role="alert">
          {error}
        </p>
      ) : null}
    </article>
  );
}

/**
 * 告诉你 (§4.2, D-060 §11.5): what Main told the owner with `owner notify`,
 * in Main's voice (never the owner's bubble): the title, the body's first 2
 * lines, 「知道了」 and 「打开对话」. 知道了 acknowledges on the Hub, so the
 * card leaves on every device; it waits for the Hub's answer (§4.4) and
 * greys out while the queue is stale (§8). Not counted in N.
 */
function NoticeCard({
  card,
  focused,
  inert,
  stale,
  nowMs,
  linkProps,
}: {
  card: Extract<HomeQueueCard, { type: "notice" }>;
  focused: boolean;
  inert: boolean;
  stale: boolean;
  nowMs: number;
  linkProps: object;
}) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { notice } = card;
  const chapter = notice.chapterInstanceId;
  return (
    <article
      className={`${css.card} ${focused ? css.cardFocus : ""}`}
      data-testid="home-queue-card"
      data-card-type="notice"
      data-card-id={notice.noticeId}
      data-notice-id={notice.noticeId}
      data-focus={focused ? "true" : "false"}
      data-stale={inert ? "1" : undefined}
    >
      <header className={css.head}>
        <span className={css.label}>告诉你 · {noticeVoice(notice)}</span>
        <span className={css.where}>{formatListTime(notice.createdAt, nowMs)}</span>
      </header>
      <p className={css.planTitle} title={notice.title} data-testid="home-card-title">
        {notice.title}
      </p>
      <p className={css.body} title={notice.body} data-testid="home-card-body">
        {notice.body}
      </p>
      <div className={css.actions}>
        <button
          type="button"
          className={css.chip}
          data-testid="home-card-ack"
          disabled={inert || busy || stale}
          onClick={() => {
            setBusy(true);
            setError(null);
            noticeStore.ack(notice.noticeId).then(
              () => setBusy(false),
              (reason: unknown) => {
                setBusy(false);
                setError(reason instanceof Error ? reason.message : String(reason));
              },
            );
          }}
        >
          知道了
        </button>
        {chapter ? (
          <Link to={`/s/${chapter}`} className={css.chipLink} data-testid="home-card-open" {...linkProps}>
            打开对话
          </Link>
        ) : null}
        {stale ? (
          <span className={css.hint} data-testid="home-card-stale">
            {HOME_STALE_TEXT}
          </span>
        ) : null}
      </div>
      {error ? (
        <p className={css.hint} role="alert">
          {error}
        </p>
      ) : null}
    </article>
  );
}
