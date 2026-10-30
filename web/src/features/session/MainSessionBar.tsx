import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { api, observationText, type LineageRecord } from "../../lib/api";
import { formatClock, formatListTime } from "../../lib/format";
import { hubStore, useHub, useMainEntry, type MainEntry } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Instance } from "../../types/instance";
import type { Observation } from "../../types/observation";
import type { Id, U64 } from "../../types/wire";
import { useMainTail } from "../home/mainTail";
import { useLineageSync } from "../home/useMain";
import {
  causeLabel,
  lastActivityOf,
  lineageFor,
  lineageStateLabel,
  mainStatus,
  restartsIn24h,
  type MainStatus,
} from "../home/mainStatus";
import { ComposerConfirmDialog } from "./ComposerOptions";
import css from "./mainSessionBar.module.css";

/** Journal rows an expanded earlier chapter loads (on demand only). */
const CHAPTER_TAIL = 200;

/**
 * D-057 §9 / ui-spec §2.2 「Main 会话」 on the shared /s/:id page: the status
 * line (chapter, lifecycle and activity, restarts in 24 h with causes, last
 * activity, lineage state), the Human-device controls — Stop (instance.cancel,
 * no lineage change), Pause (close the current chapter), Resume (continuation
 * resume, only while paused) — and the chapters of the one conversation, in
 * order, earlier ones collapsed and read on demand. Renders only on a chapter
 * of the resolved Main lineage.
 *
 * The PWA signs in only as a Human device (login defaults the device kind;
 * Bot and Agent credentials have no PWA session), so every PWA session gets
 * the controls; the Hub still enforces the origin on each verb.
 */
export function MainSessionBar({ instance, mobile }: { instance: Instance; mobile: boolean }) {
  const entry = useMainEntry();
  const lineageId = instance.lineageId ?? instance.id;
  const ours = entry !== null && entry.lineageId === lineageId;
  // Sync only while this page belongs to Main's lineage.
  useLineageSync(ours ? entry : null);
  if (!entry || !ours) return null;
  return <MainSessionBarBody entry={entry} instance={instance} mobile={mobile} />;
}

function MainSessionBarBody({ entry, instance, mobile }: { entry: MainEntry; instance: Instance; mobile: boolean }) {
  const hub = useHub();
  const nowMs = useNowTick();
  const navigate = useNavigate();
  // Only a projection read for this chapter speaks for it (r2 item 3).
  const lineage = lineageFor(hub.lineages[entry.lineageId], entry);
  const current = entry.instance;
  const status = mainStatus({
    entry,
    lineage,
    host: hub.hosts.find((row) => row.id === current.hostId),
    hostName: hubStore.hostName(current.hostId),
    asking: hub.interactions.some((row) => row.instanceId === current.id && row.state === "pending"),
    summary: hub.summaries[current.id],
    nowMs,
  });
  // 最近活动: the last message or turn observation, never the row's
  // updatedAt (every journal append rewrites it). The current chapter's
  // followed journal answers when this page follows it; otherwise the same
  // bounded tail the home card reads.
  const followed = hub.events[current.id];
  const { tail } = useMainTail(entry, hub.instances, followed === undefined);
  const lastActivityAt = followed ? lastActivityOf(followed) : (tail?.lastActivityAt ?? null);
  const [busy, setBusy] = useState<"stop" | "pause" | "resume" | null>(null);
  const [confirmPause, setConfirmPause] = useState(false);
  // This page is an earlier chapter: say so and link to the current one.
  const superseded = instance.id !== current.id;

  const run = async (kind: "stop" | "pause" | "resume") => {
    setBusy(kind);
    try {
      if (kind === "stop") await hubStore.cancel(current.id as Id);
      else if (kind === "pause") await hubStore.close(current.id as Id);
      else {
        const next = await hubStore.resume(current.id as Id, "structured");
        if (next) navigate(`/s/${next}/structured`);
      }
    } catch (error) {
      hubStore.toast(error instanceof Error ? error.message : "操作失败");
    } finally {
      setBusy(null);
    }
  };

  const restarts = restartsIn24h(lineage, nowMs);
  const stateLabel = lineageStateLabel(lineage, status);
  const turnBusy = status.phase === "working" || status.phase === "asking";

  return (
    <div className={css.bar} data-testid="main-session-bar" data-phase={status.phase} data-superseded={superseded ? "1" : "0"}>
      <div className={css.statusLine} data-testid="main-status-line">
        <span className={css.lead}>Main</span>
        <span className={css.item} data-testid="main-status-chapter">
          第 {entry.generation} 章
        </span>
        <span className={css.item} data-testid="main-status-phase" data-tone={status.tone}>
          {lifecycleWords(current, status)}
        </span>
        {restarts ? (
          <span className={css.item} data-testid="main-status-restarts" title={restartTitle(restarts)}>
            {restarts.count === 0
              ? "24 小时内无重启"
              : `24 小时内重启 ${restarts.count} 次（${restarts.causes.map((c) => `${c.cause}×${c.count}`).join("、")}）`}
          </span>
        ) : null}
        <span className={css.item} data-testid="main-status-last-activity">
          最近活动 {lastActivityAt ? formatListTime(lastActivityAt, nowMs) : "—"}
        </span>
        {stateLabel ? (
          <span className={css.item} data-testid="main-status-lineage" data-tone={status.tone}>
            {stateLabel}
          </span>
        ) : null}
        <span className={css.controls}>
          {superseded ? (
            <Link className={css.btn} data-testid="main-open-current" to={`/s/${current.id}`}>
              打开当前 Main
            </Link>
          ) : null}
          {entry.live && !superseded && turnBusy ? (
            <button
              type="button"
              className={css.btn}
              data-testid="main-stop"
              disabled={busy !== null}
              title="打断当前回合（不改变血缘状态）"
              onClick={() => void run("stop")}
            >
              {busy === "stop" ? "打断中…" : "打断"}
            </button>
          ) : null}
          {entry.live && !superseded ? (
            <button
              type="button"
              className={css.btn}
              data-testid="main-pause"
              disabled={busy !== null}
              title="关闭当前章节，血缘暂停"
              onClick={() => setConfirmPause(true)}
            >
              {busy === "pause" ? "暂停中…" : "暂停"}
            </button>
          ) : null}
          {status.resumable ? (
            <button
              type="button"
              className={`${css.btn} ${css.btnPrimary}`}
              data-testid="main-resume"
              disabled={busy !== null}
              title="续接下一章"
              onClick={() => void run("resume")}
            >
              {busy === "resume" ? "恢复中…" : "恢复"}
            </button>
          ) : null}
        </span>
      </div>
      <MainChapters lineage={lineage} entry={entry} viewing={instance.id} instances={hub.instances} />
      <ComposerConfirmDialog
        open={confirmPause}
        variant={mobile ? "sheet" : "popover"}
        title="暂停 Main"
        message="关闭当前章节，Main 不会自动重启，直到你恢复。正在进行的回合会中止。"
        confirmLabel="暂停"
        onCancel={() => setConfirmPause(false)}
        onConfirm={() => {
          setConfirmPause(false);
          void run("pause");
        }}
      />
    </div>
  );
}

const LIFECYCLE_WORD: Partial<Record<Instance["lifecycle"], string>> = {
  requested: "已请求",
  preparing: "准备中",
  starting: "启动中",
  ready: "运行中",
  running: "运行中",
  closing: "关闭中",
  exited: "已退出",
  failed: "启动失败",
};

/** Lifecycle and activity in words (no wire triple in the default view). */
function lifecycleWords(instance: Instance, status: MainStatus): string {
  const lifecycle = LIFECYCLE_WORD[instance.lifecycle] ?? "状态待确认";
  if (status.phase === "working" || status.phase === "asking" || status.phase === "idle") {
    return `${lifecycle} · ${status.phrase}`;
  }
  return lifecycle;
}

function restartTitle(restarts: { count: number; causes: { cause: string; count: number }[] }): string {
  return restarts.causes.map((c) => `${c.cause} ×${c.count}`).join("\n") || "24 小时内无重启";
}

type ChapterRow = LineageRecord["chapters"][number];

/**
 * The lineage's chapters in order, each behind a divider: cause and time.
 * Earlier chapters stay collapsed and load their messages only when opened.
 * Fence outcomes and superseded questions are not rendered: the Hub does not
 * serve them on the lineage read yet (ma-fence).
 */
function MainChapters({
  lineage,
  entry,
  viewing,
  instances,
}: {
  lineage: LineageRecord | null;
  entry: MainEntry;
  viewing: string;
  instances: Instance[];
}) {
  const [open, setOpen] = useState(false);
  const chapters = lineage?.chapters ?? [];
  const earlier = chapters.filter((row) => row.instanceId !== entry.instance.id);
  if (earlier.length === 0) return null;
  return (
    <div className={css.chapters} data-testid="main-chapters">
      <button
        type="button"
        className={css.chaptersToggle}
        data-testid="main-chapters-toggle"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        {open ? "▾" : "▸"} 较早的 {earlier.length} 章
      </button>
      {open ? (
        <ol className={css.chapterList}>
          {chapters.map((chapter) => (
            <ChapterDivider
              key={chapter.instanceId}
              chapter={chapter}
              current={chapter.instanceId === entry.instance.id}
              viewing={chapter.instanceId === viewing}
              instance={instances.find((row) => row.id === chapter.instanceId) ?? null}
            />
          ))}
        </ol>
      ) : null}
    </div>
  );
}

function ChapterDivider({
  chapter,
  current,
  viewing,
  instance,
}: {
  chapter: ChapterRow;
  current: boolean;
  viewing: boolean;
  instance: Instance | null;
}) {
  const [messages, setMessages] = useState<Observation[] | null>(null);
  const [state, setState] = useState<"idle" | "loading" | "error">("idle");
  const load = async () => {
    if (!instance) return;
    setState("loading");
    try {
      const from = Math.max(0, Number(instance.durableSeq ?? "0") - CHAPTER_TAIL);
      const page = await api.eventsRead({
        journalId: instance.journalId,
        afterSeq: String(from) as U64,
        limit: CHAPTER_TAIL,
      });
      setMessages(page.events.filter((event) => event.kind === "message"));
      setState("idle");
    } catch {
      setState("error");
    }
  };
  const span = `${formatClock(chapter.createdAt)}${chapter.endedAt ? ` – ${formatClock(chapter.endedAt)}` : ""}`;
  return (
    <li className={css.chapter} data-testid="main-chapter" data-generation={chapter.generation} data-current={current ? "1" : "0"}>
      <div className={css.divider} data-testid="main-chapter-divider">
        <span className={css.dividerRule} aria-hidden="true" />
        <span className={css.dividerText}>
          第 {chapter.generation} 章 · {causeLabel(chapter.chapterCause)} · {span}
          {current ? " · 当前" : ""}
          {viewing && !current ? " · 正在查看" : ""}
        </span>
        <span className={css.dividerRule} aria-hidden="true" />
      </div>
      {current ? null : messages ? (
        <ul className={css.chapterMessages} data-testid="main-chapter-messages">
          {messages.length === 0 ? <li className={css.chapterEmpty}>这一章没有可显示的消息</li> : null}
          {messages.map((event) => {
            const role = (event.payload as { role?: string }).role;
            return (
              <li key={String(event.seq)} className={css.chapterMessage} data-role={role}>
                <span className={css.chapterRole}>{role === "assistant" ? "Main" : role === "user" ? "你" : role ?? "—"}</span>
                <span className={css.chapterText}>{observationText(event)}</span>
              </li>
            );
          })}
        </ul>
      ) : instance ? (
        <button
          type="button"
          className={css.chapterLoad}
          data-testid="main-chapter-load"
          disabled={state === "loading"}
          onClick={() => void load()}
        >
          {state === "loading" ? "加载中…" : state === "error" ? "加载失败，重试" : "展开这一章"}
        </button>
      ) : (
        <Link className={css.chapterLoad} to={`/s/${chapter.instanceId}`}>
          打开这一章
        </Link>
      )}
    </li>
  );
}
