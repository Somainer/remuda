import { useEffect, useMemo } from "react";
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { knowledgeValue } from "../../../types/command";
import { formatTokens } from "../../../lib/format";
import type { ToolCallPayload } from "../../../types/generated";
import {
  expectedTiersFor,
  channelHealth,
  type TierHealth,
} from "./channelHealth";
import { isActivePhase, livePhase, toolAnchors, toolFingerprint, type LivePhaseName } from "./phase";
import { liveStatus, phraseIsThinking } from "./liveStatus";
import { projectTurnDecision, turnStartAnchor } from "./turnDecision";
import type { TurnDecision } from "./turnEnd";
import { usageOutputTokens } from "./liveTokens";
import { formatElapsed, publishToolLive, resetToolLive, useElapsed, useNow, useToolElapsed } from "./useElapsed";
import css from "./live.module.css";

/** Chinese status copy; the wire spelling stays in `data-phase`. */
const PHASE_LABEL: Record<LivePhaseName, string> = {
  "prompt-accepted": "已接收",
  thinking: "思考中",
  "tool-started": "工具运行中",
  "tool-output": "工具输出中",
  "tool-finished": "工具完成",
  "text-streaming": "文本生成中",
  "turn-ended": "回合结束",
  blocked: "等待操作",
  interrupted: "已打断",
};

/** Screen-only pseudo-phases (no hook phase latched). */
const SCREEN_LABEL: Record<string, string> = {
  thinking: "思考中",
  working: "工作中",
};

const OUTCOME_LABEL: Record<string, string> = {
  completed: "完成",
  cancelled: "已取消",
  failed: "失败",
};

const HEALTH_COPY: Record<Exclude<TierHealth["reason"], "ok" | "disabled">, string> = {
  // The anchor channel went quiet: the elapsed reading greys, never freezes
  // into a claim that the turn ended.
  stalled: "通道静默，计时可能不准",
  // D-4: an expected tier that never produced a record is a failure mode,
  // not a quiet agent.
  "never-materialised": "该通道始终没有记录",
};

/**
 * The Node-side check that explains a quiet hook tier, appended to the amber
 * note only when the instance actually surfaced one. The spelling is the
 * stable id from `remuda_node::hook_silence`; absent a check the badge keeps
 * exactly the copy above.
 */
const SILENCE_REASON_COPY: Record<string, string> = {
  "relay-missing": "钩子 relay 缺失或不可执行",
  "socket-refused": "钩子 socket 无监听",
  "link-stalled": "journal 推送停滞",
};

function HealthNote({ health, silenceReason }: { health: TierHealth; silenceReason?: string | null }) {
  if (health.reason === "ok" || health.reason === "disabled") return null;
  const detail = silenceReason ? SILENCE_REASON_COPY[silenceReason] : null;
  return (
    <span
      className={health.reason === "stalled" ? css.warnStalled : css.warnMissing}
      data-testid={`live-health-${health.tier}`}
      data-reason={health.reason}
      data-silence={silenceReason ?? undefined}
    >
      {health.tier} · {HEALTH_COPY[health.reason]}
      {detail ? ` · ${detail}` : null}
    </span>
  );
}

/**
 * The newest Node-reported hook-silence check for this session, if one was
 * surfaced as a `hook.silence` diagnostic. The web never guesses the cause; it
 * only names a check the Node actually ran (`relay-missing` / `socket-refused`
 * / `link-stalled`).
 */
function hookSilenceReason(events: readonly Observation[]): string | null {
  for (let i = events.length - 1; i >= 0; i -= 1) {
    const ev = events[i]!;
    if (ev.kind !== "lifecycle" || ev.payload.type !== "native") continue;
    if (ev.payload.nativeName !== "hook.silence") continue;
    const reason = ev.payload.relatedIds?.reason;
    if (reason && reason in SILENCE_REASON_COPY) return reason;
  }
  return null;
}

/** Earliest parseable anchor: hook `since` and the screen re-anchor. */
function earliestAnchor(a: string | null | undefined, b: string | null | undefined): string | null {
  let best: string | null = null;
  for (const candidate of [a, b]) {
    if (!candidate) continue;
    const t = Date.parse(candidate);
    if (Number.isNaN(t)) continue;
    if (best == null || t < Date.parse(best)) best = candidate;
  }
  return best;
}

/**
 * The live status strip: one phase label · the spinner verb · one elapsed
 * reading · the streamed token count · tier · the muted spinner phrase ·
 * channel-health notes. It renders *status only*: message text and tool
 * content live in the transcript and never reach this component (live-view
 * design §2.4).
 *
 * The spinner fields come from the screen tier (claude's own status line);
 * token counts prefer hook/transcript usage and fall back to that screen
 * number — the two are never shown together.
 */
export function LiveStatusStrip({
  events,
  nativeRef,
  onInterrupt,
  hasPending = false,
  decision: decisionProp,
}: {
  events: readonly Observation[];
  nativeRef: NativeRef | null | undefined;
  onInterrupt?: () => void;
  /** A real dialog/permission is pending for this instance. */
  hasPending?: boolean;
  /** The folded turn decision; computed internally when not supplied. */
  decision?: TurnDecision;
}) {
  const expectedTiers = useMemo(() => expectedTiersFor(nativeRef), [nativeRef]);
  const phase = useMemo(() => livePhase(events), [events]);
  const status = useMemo(() => liveStatus(events), [events]);
  const usageCount = useMemo(() => usageOutputTokens(events), [events]);
  const anchors = useMemo(() => toolAnchors(events), [events]);
  // Health is a function of wall time; the 1 Hz clock recomputes staleness,
  // and a note must be able to appear before the first phase lands.
  const now = useNow(true);
  const health = useMemo(
    () => channelHealth(events, expectedTiers, now),
    [events, expectedTiers, now],
  );
  // The turn-end decision folds every channel (hook latch, the screen, the
  // transcript tail, pending interactions); the strip renders it, not the raw
  // hook latch. SessionPage shares the same reducer so the composer's
  // working→idle flush boundary is the exact decision painted here.
  const decision = useMemo(
    () => decisionProp ?? projectTurnDecision(events, nativeRef, hasPending, now),
    [decisionProp, events, nativeRef, hasPending, now],
  );
  const silenceReason = useMemo(() => hookSilenceReason(events), [events]);

  // Feed the running tool cards their anchors. This is the only bridge
  // between the event list and the virtualised transcript rows.
  useEffect(() => {
    publishToolLive(anchors, health);
  }, [anchors, health]);
  useEffect(() => () => resetToolLive(), []);

  const notes = [...health.values()].filter((item) => item.reason !== "ok" && item.reason !== "disabled");
  const screenActive = status?.active === true;
  const ended = decision.state === "ended";
  const waiting = decision.state === "waiting";
  // The spinner phrase ("thinking with xhigh effort") distinguishes the
  // reasoning wait from a plain prompt-accepted gap, where hooks emit no
  // thinking phase of their own.
  const thinking =
    !ended &&
    !waiting &&
    screenActive &&
    phraseIsThinking(status?.phrase) &&
    (!phase || phase.phase === "thinking" || phase.phase === "prompt-accepted");
  // Whether the turn clock keeps running. On an end it stops at `endedAt`; a
  // genuinely open (working/waiting) turn ticks; `unknown` falls back to the
  // last raw evidence so the reading stays visible but greys.
  const rawActive = phase ? isActivePhase(phase.phase) : screenActive;
  const active = ended ? false : decision.state === "working" || waiting ? true : rawActive;
  // Hooks have no thinking channel for claude: prompt-accepted stays latched
  // while the model reasons. The screen phrase is the one channel that says
  // so ("thinking with xhigh effort"), so it promotes the label to 思考中
  // (and the transcript gets a live thinking row). Every other phase stays
  // the hook's authoritative word.
  const pseudoPhase: string | null = ended
    ? "turn-ended"
    : waiting
      ? "blocked"
      : thinking
        ? "thinking"
        : decision.state === "working"
          ? (phase?.phase ?? (screenActive ? "working" : null))
          : phase
            ? phase.phase
            : screenActive
              ? "working"
              : null;
  // The clock always anchors at the turn *start* (the submit/spinner re-anchor),
  // never at the end: on an end it freezes on the duration (endedAt − start),
  // which is what the terminal shows, instead of collapsing to 0:00 or drifting
  // as time-since-end on a page opened later.
  const startAnchor = useMemo(() => turnStartAnchor(events), [events]);
  const anchor = startAnchor ?? earliestAnchor(phase?.since, status?.since);
  const phaseHealth = phase?.tier ? health.get(phase.tier) : undefined;
  const liveElapsed = useElapsed(anchor, active, phaseHealth);
  const elapsed = useMemo(() => {
    if (!ended) return liveElapsed;
    const start = Date.parse(anchor ?? "");
    const end = Date.parse(decision.endedAt ?? "");
    if (Number.isNaN(start) || Number.isNaN(end) || end < start) return liveElapsed;
    const ms = end - start;
    return { ms, text: formatElapsed(ms), stale: false };
  }, [ended, liveElapsed, anchor, decision.endedAt]);
  if (!ended && !waiting && !phase && !screenActive && notes.length === 0) return null;

  const phaseLabel = ended
    ? "回合结束"
    : (pseudoPhase && PHASE_LABEL[pseudoPhase as LivePhaseName]) ||
      SCREEN_LABEL[pseudoPhase ?? "working"] ||
      "工作中";
  const worst = notes[0]?.reason ?? "ok";

  // One token count, one source: real usage once it lands, otherwise the
  // screen's live estimate. Hidden once the turn ended.
  const tokenLabel = ended
    ? null
    : usageCount != null
      ? formatTokens({ state: "known", value: String(usageCount) })
      : status?.tokensLabel ?? null;

  const phrase = ended ? null : status?.phrase ?? phase?.phrase ?? null;
  // Esc belongs only to a turn we know is live. An ended turn and an unknown
  // state (a stale hook latch) never show it; a blocked dialog is owned by the
  // answer keys.
  const canInterrupt =
    decision.state === "working" && phase?.phase !== "blocked" && (status ? status.interruptible : true);

  return (
    <div
      className={css.strip}
      data-testid="live-status-strip"
      data-phase={pseudoPhase}
      data-turn={decision.state}
      data-decided-by={ended ? decision.decidedBy : undefined}
      data-health={worst}
    >
      <span className={css.phase} data-testid="live-phase">
        <span className={active ? css.liveDot : css.idleDot} aria-hidden />
        {phaseLabel}
        {ended && phase?.phase === "turn-ended" && phase.outcome ? ` · ${OUTCOME_LABEL[phase.outcome] ?? phase.outcome}` : null}
        {!ended && phase?.toolName ? <span className={css.toolName}>{phase.toolName}</span> : null}
      </span>
      {ended && decision.decidedBy ? (
        // Names the channel that decided the end (hook / file / screen /
        // transcript), so a screen-decided end with no Stop hook, or a grok
        // file-tier end, is never mistaken for a hook-decided one.
        <span className={css.chip} data-testid="live-decided-by" data-channel={decision.decidedBy}>
          {decision.decidedBy}
        </span>
      ) : null}
      {status?.verb && active ? (
        <span className={css.verb} data-testid="live-verb">
          {status.verb}…
        </span>
      ) : null}
      {elapsed ? (
        <span
          className={elapsed.stale ? css.elapsedStale : css.elapsed}
          data-testid="live-elapsed"
          data-stale={elapsed.stale ? "1" : "0"}
          title={
            ended
              ? "回合结束，计时停在该回合时长"
              : elapsed.stale
                ? "锚定该状态的通道已静默：时间仍在累计，但状态可能已经变化"
                : "自 harness 报告该状态起经过的时间，本地 1 Hz 计时"
          }
        >
          {elapsed.text}
        </span>
      ) : null}
      {active && tokenLabel ? (
        <span className={css.tokenCount} data-testid="live-token-count" data-source={usageCount != null ? "usage" : "screen"}>
          ↓ {tokenLabel} tokens
        </span>
      ) : null}
      {!ended && (phase?.tier || screenActive) ? (
        <span className={css.chip} data-testid="live-tier">
          {phase?.tier ?? "screen"}
          {phase?.provision && phase.provision !== "native" ? ` · ${phase.provision}` : null}
        </span>
      ) : null}
      {phrase ? (
        // Spinner phrase: screen status, refreshed at most once per change.
        // Never parsed for meaning beyond the thinking label; the structured
        // phase is authoritative.
        <span className={css.phrase} data-testid="live-phrase" data-thinking={thinking ? "1" : "0"}>
          {phrase}
        </span>
      ) : null}
      {canInterrupt && onInterrupt ? (
        <button
          type="button"
          className={css.interrupt}
          data-testid="live-interrupt"
          title="向终端发送 Esc，打断当前 turn"
          onClick={onInterrupt}
        >
          Esc 打断
        </button>
      ) : null}
      {notes.map((item) => (
        <HealthNote
          key={item.tier}
          health={item}
          silenceReason={item.tier === "hook" ? silenceReason : null}
        />
      ))}
    </div>
  );
}

/**
 * Running-Bash status: the existing `running · 无 exit` wording plus the one
 * elapsed reading that exists in the UI. Renders no elapsed before the
 * tool-start anchor is projected, and is not mounted once the result is
 * Final — the tick stops exactly there.
 */
export function LiveToolElapsed({ call }: { call: ToolCallPayload }) {
  const running = true;
  const fingerprint = toolFingerprint(knowledgeValue(call.toolName), knowledgeValue(call.input));
  const elapsed = useToolElapsed(fingerprint, running);
  return (
    <>
      running · 无 exit，不画成功
      {elapsed ? (
        <>
          {" · "}
          <span
            className={elapsed.stale ? css.elapsedStale : css.elapsed}
            data-testid="tool-elapsed"
            data-stale={elapsed.stale ? "1" : "0"}
          >
            {elapsed.text}
          </span>
        </>
      ) : null}
    </>
  );
}
