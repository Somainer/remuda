import type { LineageRecord } from "../../lib/api";
import { observationText } from "../../lib/api";
import { formatClock, formatListTime } from "../../lib/format";
import { projectStatus } from "../../lib/status";
import type { LocalBubble, MainEntry } from "../../lib/store";
import type { Host, UiStatus } from "../../types/instance";
import type { Observation } from "../../types/observation";

/**
 * D-057 §9 Main projections shared by the home Main card, the desktop pinned
 * row and the /s/:id status line (ma-main-ui). Pure: every phrase is decided
 * by fields the Hub already serves — the resolved chapter row, its host, its
 * pending interaction, the `GET /v1/lineages/{id}` record, and the journal
 * tail. A field the Hub does not serve yet (the paused-by record and fence
 * outcomes land with ma-fence) is feature-detected and simply not shown.
 */

export type MainPhase =
  | "working"
  | "asking"
  | "idle"
  | "starting"
  | "restarting"
  | "paused"
  | "stopped"
  | "host-offline"
  | "unknown";

export type MainStatus = {
  phase: MainPhase;
  dot: UiStatus;
  /** Line-1 phrase after 「Main · 」. */
  phrase: string;
  /** amber = needs you / host away; muted = paused or stopped. */
  tone: "normal" | "attention" | "muted";
  /** True when the only way forward is Resume: the send button becomes 恢复. */
  resumable: boolean;
  /** The chapter's host has no live link: sends queue until it returns. */
  hostOffline: boolean;
  hostName: string;
};

/** Feature-detected paused-by record (written by ma-fence; absent before). */
export type PausedBy = { by: string | null; at: string | null };

export function lineagePausedBy(lineage: LineageRecord | undefined | null): PausedBy | null {
  if (!lineage) return null;
  const raw = lineage.pausedBy;
  const pausedAt = typeof lineage.pausedAt === "string" ? lineage.pausedAt : null;
  if (raw == null) return lineage.state === "paused" ? { by: null, at: pausedAt } : null;
  if (typeof raw === "string") return { by: raw, at: pausedAt };
  if (typeof raw === "object") {
    const record = raw as Record<string, unknown>;
    const by =
      typeof record.by === "string" ? record.by : typeof record.kind === "string" ? record.kind : null;
    const at = typeof record.at === "string" ? record.at : pausedAt;
    return { by, at };
  }
  return { by: null, at: pausedAt };
}

/** 「已由你暂停 · T」 and friends (main-agent §6.2 paused.by values). */
export function pausedLabel(paused: PausedBy, nowMs: number): string {
  const at = paused.at ? ` · ${formatListTime(paused.at, nowMs)}` : "";
  switch (paused.by) {
    case "device":
      return `已由你暂停${at}`;
    case "self":
      return `Main 已自行关闭${at}`;
    case "ancestor":
      return `已由上级暂停${at}`;
    case "restart-cap":
      return "已暂停 · 重启上限";
    case "process-exit":
      return `已暂停 · 进程退出${at}`;
    default:
      return `已暂停${at}`;
  }
}

/** Compact elapsed span (home.md §6.2): 「3分」「2h」「4d」. */
export function sinceLabel(iso: string | null | undefined, nowMs: number): string | null {
  if (!iso) return null;
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return null;
  const minutes = Math.max(0, Math.floor((nowMs - t) / 60_000));
  if (minutes < 1) return "<1分";
  if (minutes < 60) return `${minutes}分`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

const CAUSE_LABEL: Record<string, string> = {
  "owner-resume": "你恢复",
  "process-exit": "进程退出",
  "process-loss": "进程退出",
  "start-failed": "启动失败",
  "node-restarted": "Node 重启",
};

/**
 * The C1 restart causes (main-agent.md §6.3): process loss, start failure and
 * node restart. Only these count as restarts; an owner resume is the owner's
 * own action, and a spelling not listed here is never counted. The Hub writes
 * only `owner-resume` until ma-restart lands; §6.3 names `start-failed` and
 * `node-restarted`, and leaves the process-loss row unspelled, so both the
 * restart-policy word (`process-loss`) and the paused-by word (`process-exit`)
 * are accepted for it.
 */
const C1_CAUSES: ReadonlySet<string> = new Set(["process-loss", "process-exit", "start-failed", "node-restarted"]);

export function isRestartCause(cause: string | null | undefined): boolean {
  return cause != null && C1_CAUSES.has(cause);
}

/** A chapter cause in words; unknown spellings stay verbatim (never guessed). */
export function causeLabel(cause: string | null | undefined): string {
  if (!cause) return "首章";
  return CAUSE_LABEL[cause] ?? cause;
}

function hostIsOffline(host: Host | undefined): boolean {
  if (!host) return false;
  return host.online === false || host.state === "offline";
}

export type MainStatusInput = {
  entry: MainEntry;
  lineage?: LineageRecord | null;
  host?: Host;
  hostName: string;
  /** The Main chapter has a pending interaction (question / approval). */
  asking: boolean;
  /** The list-row live phrase for the chapter, when one was projected. */
  summary?: string;
  nowMs: number;
};

/**
 * The lineage projection only speaks for the chapter it was read for: one
 * whose generation is older than the resolved chapter predates a chapter
 * switch and is ignored (the instance rows decide instead).
 */
export function lineageFor(lineage: LineageRecord | null | undefined, entry: MainEntry): LineageRecord | null {
  if (!lineage) return null;
  return lineage.generation >= entry.generation ? lineage : null;
}

export function mainStatus(input: MainStatusInput): MainStatus {
  const { entry, host, hostName, asking, summary, nowMs } = input;
  const lineage = lineageFor(input.lineage, entry);
  const instance = entry.instance;
  const base = { hostName, hostOffline: false, resumable: false };
  // Live instance evidence wins over a cached paused record: 恢复 is never
  // offered for a chapter that is running.
  const paused = entry.live ? null : lineagePausedBy(lineage);
  if (paused) {
    return { ...base, phase: "paused", dot: "exited", phrase: pausedLabel(paused, nowMs), tone: "muted", resumable: true };
  }
  if (!entry.live && lineage && lineage.generation > entry.generation) {
    // The lineage already holds a successor the instance list has not shown
    // yet (a resume from another device, a restart): it is starting, not
    // stopped, and resuming again would open yet another chapter.
    return { ...base, phase: "starting", dot: "starting", phrase: "正在启动下一章", tone: "normal" };
  }
  if (!entry.live) {
    // No live chapter and no paused-by record: the chapter ended (a Pause
    // before ma-fence records it, or a process end with no supervisor yet).
    // Only Resume continues the lineage; the label names no actor.
    const chapter = lineage?.chapters.find((row) => row.instanceId === instance.id);
    const at = chapter?.endedAt ?? instance.updatedAt;
    const when = at ? ` · ${formatListTime(at, nowMs)}` : "";
    return { ...base, phase: "stopped", dot: "exited", phrase: `已停止${when}`, tone: "muted", resumable: true };
  }
  const offline =
    hostIsOffline(host) || lineage?.state === "host-offline" || instance.connectivity === "disconnected";
  if (offline) {
    const span = sinceLabel(host?.lastSeenAt ?? null, nowMs);
    return {
      ...base,
      hostOffline: true,
      phase: "host-offline",
      dot: "unknown",
      phrase: `主机 ${hostName} 离线${span ? ` · ${span}` : ""}`,
      tone: "attention",
    };
  }
  if (asking) {
    return { ...base, phase: "asking", dot: "blocked", phrase: "在问你", tone: "attention" };
  }
  const status = projectStatus(instance);
  if (status === "starting") {
    const restarting = entry.generation > 1 && isRestartCause(instance.chapterCause);
    return {
      ...base,
      phase: restarting ? "restarting" : "starting",
      dot: "starting",
      phrase: restarting ? "正在重启" : "正在启动",
      tone: "normal",
    };
  }
  if (status === "working") {
    const live = summary?.trim();
    return { ...base, phase: "working", dot: "working", phrase: live ? `工作中 · ${live}` : "工作中", tone: "normal" };
  }
  if (status === "idle") {
    const clock = instance.updatedAt ? formatClock(instance.updatedAt) : null;
    return { ...base, phase: "idle", dot: "idle", phrase: clock && clock !== "—" ? `空闲 · ${clock}` : "空闲", tone: "normal" };
  }
  if (status === "blocked") {
    return { ...base, phase: "asking", dot: "blocked", phrase: "在问你", tone: "attention" };
  }
  return { ...base, phase: "unknown", dot: "unknown", phrase: "状态待确认", tone: "normal" };
}

/**
 * C1 restarts in the last 24 h with their causes (status line). Owner resumes
 * and unknown causes are not restarts. `null` until the lineage record is
 * read — never a fabricated zero.
 */
export function restartsIn24h(
  lineage: LineageRecord | null | undefined,
  nowMs: number,
): { count: number; causes: { cause: string; count: number }[] } | null {
  if (!lineage) return null;
  const since = nowMs - 24 * 60 * 60 * 1000;
  const counts = new Map<string, number>();
  let count = 0;
  for (const chapter of lineage.chapters) {
    if (chapter.generation <= 1 || !isRestartCause(chapter.chapterCause)) continue;
    const t = Date.parse(chapter.createdAt);
    if (!Number.isFinite(t) || t < since) continue;
    count += 1;
    const label = causeLabel(chapter.chapterCause);
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  return { count, causes: [...counts].map(([cause, n]) => ({ cause, count: n })) };
}

/** The lineage state word for the status line (§9). */
export function lineageStateLabel(
  lineage: LineageRecord | null | undefined,
  status: MainStatus,
): string | null {
  if (status.phase === "paused" || status.phase === "stopped") return status.phrase;
  if (status.phase === "host-offline") {
    const since = status.phrase.includes("·") ? status.phrase.split(" · ").at(-1) : null;
    return since ? `主机离线，已 ${since}` : "主机离线";
  }
  if (status.phase === "restarting") return "正在重启";
  if (status.phase === "starting") return status.phrase;
  if (!lineage) return null;
  if (lineage.state === "running") return "运行中";
  if (lineage.state === "starting") return "启动中";
  // "paused" on a live chapter is a stale read (live evidence wins); other
  // words are shown only through the phases above.
  return null;
}

// ── line 2: the last reply, or the echo of the latest send ────────────────

export type MainReply = { text: string; seq: string };

export type MainTail = {
  /** Newest assistant message of the tail, first non-empty line. */
  reply: MainReply | null;
  /** seq of the user message each commandId settled into, when in the tail. */
  userSeqByCommand: Map<string, string>;
  /** Assistant messages in tail order (to find the reply after a send). */
  replies: MainReply[];
};

export function firstLine(text: string): string {
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.replace(/\s+/g, " ").trim();
    if (trimmed) return trimmed;
  }
  return "";
}

/** Fold a journal tail (ascending) into what line 2 needs. */
export function readMainTail(events: readonly Observation[]): MainTail {
  const replies: MainReply[] = [];
  const userSeqByCommand = new Map<string, string>();
  for (const event of events) {
    if (event.kind !== "message") continue;
    const payload = event.payload as { role?: string; commandId?: string };
    if (payload.role === "assistant") {
      const text = firstLine(observationText(event));
      if (text) replies.push({ text, seq: String(event.seq) });
    } else if (payload.role === "user" && typeof payload.commandId === "string") {
      userSeqByCommand.set(payload.commandId, String(event.seq));
    }
  }
  return { reply: replies.at(-1) ?? null, userSeqByCommand, replies };
}

export type MainEcho =
  | { kind: "reply"; text: string }
  | { kind: "empty" }
  | { kind: "sending"; text: string }
  | { kind: "queued"; text: string }
  | { kind: "rejected"; text: string; reason: string | null; superseded: boolean; bubble: LocalBubble }
  | { kind: "unknown"; text: string; bubble: LocalBubble };

const SENDING = "已发送 · Main 正在处理…";

function seqAfter(a: string, b: string): boolean {
  try {
    return BigInt(a) > BigInt(b);
  } catch {
    return Number(a) > Number(b);
  }
}

/**
 * Line 2 of the Main card. The latest send from this device wins until the
 * journal proves Main answered it: a reply whose seq follows the user message
 * that send settled into. Journal order, never device clocks.
 */
export function mainEcho(input: {
  entry: MainEntry;
  bubbles: readonly LocalBubble[];
  tail: MainTail | null;
  /** Main's current chapter is busy (working / asking). */
  busy: boolean;
  /** The connection to the Hub is live. */
  online: boolean;
  /** The newest reply an earlier chapter showed, for a chapter with none yet. */
  carriedReply?: string | null;
}): MainEcho {
  const { entry, bubbles, tail, busy, online, carriedReply } = input;
  const lineage = new Set<string>([entry.instance.id, ...entry.chapterIds]);
  const bubble = [...bubbles].reverse().find((b) => lineage.has(b.instanceId) && !b.held) ?? null;
  const lastText = tail?.reply?.text ?? carriedReply ?? null;
  const replyEcho: MainEcho = lastText ? { kind: "reply", text: lastText } : { kind: "empty" };
  if (!bubble) return replyEcho;
  const state = bubble.outboxState;
  if (state === "rejected") {
    // Superseded: the send addressed a chapter that is no longer Main.
    const superseded = bubble.instanceId !== entry.instance.id;
    return {
      kind: "rejected",
      text: superseded ? "未送达 · 章节已被取代" : "未送达",
      reason: bubble.outboxError ?? null,
      superseded,
      bubble,
    };
  }
  if (state === "unknown" || (state === undefined && bubble.state === "unknown")) {
    return { kind: "unknown", text: "状态待确认", bubble };
  }
  if (state === "held") return { kind: "queued", text: "已排队 · 主机离线，上线后送达" };
  if ((state === "pending" || state === "inflight") && !online) {
    return { kind: "queued", text: "待发送（离线）· 恢复连接后送达" };
  }
  const userSeq = bubble.commandId ? tail?.userSeqByCommand.get(bubble.commandId) : undefined;
  if (userSeq) {
    const answered = tail?.replies.find((reply) => seqAfter(reply.seq, userSeq));
    if (answered) return { kind: "reply", text: tail!.reply!.text };
    // The send landed; while the turn runs it is being processed, and once it
    // ended without a text reply the previous reply line stays.
    if (busy) return { kind: "sending", text: SENDING };
    return replyEcho;
  }
  if (bubble.state === "settled") return busy ? { kind: "sending", text: SENDING } : replyEcho;
  return { kind: "sending", text: SENDING };
}
