import { useEffect, useMemo } from "react";
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { knowledgeValue } from "../../../types/command";
import type { ToolCallPayload } from "../../../types/generated";
import {
  expectedTiersFor,
  channelHealth,
  type TierHealth,
} from "./channelHealth";
import { isActivePhase, livePhase, toolAnchors, toolFingerprint, type LivePhaseName } from "./phase";
import { publishToolLive, resetToolLive, useElapsed, useNow, useToolElapsed } from "./useElapsed";
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

function HealthNote({ health }: { health: TierHealth }) {
  if (health.reason === "ok" || health.reason === "disabled") return null;
  return (
    <span
      className={health.reason === "stalled" ? css.warnStalled : css.warnMissing}
      data-testid={`live-health-${health.tier}`}
      data-reason={health.reason}
    >
      {health.tier} · {HEALTH_COPY[health.reason]}
    </span>
  );
}

/**
 * The live status strip: one phase label · one elapsed reading · tier · the
 * muted spinner phrase · channel-health notes. It renders *status only*:
 * message text and tool content live in the transcript and never reach this
 * component (live-view design §2.4).
 */
export function LiveStatusStrip({
  events,
  nativeRef,
}: {
  events: readonly Observation[];
  nativeRef: NativeRef | null | undefined;
}) {
  const expectedTiers = useMemo(() => expectedTiersFor(nativeRef), [nativeRef]);
  const phase = useMemo(() => livePhase(events), [events]);
  const anchors = useMemo(() => toolAnchors(events), [events]);
  // Health is a function of wall time; the 1 Hz clock recomputes staleness,
  // and a note must be able to appear before the first phase lands.
  const now = useNow(true);
  const health = useMemo(
    () => channelHealth(events, expectedTiers, now),
    [events, expectedTiers, now],
  );

  // Feed the running tool cards their anchors. This is the only bridge
  // between the event list and the virtualised transcript rows.
  useEffect(() => {
    publishToolLive(anchors, health);
  }, [anchors, health]);
  useEffect(() => () => resetToolLive(), []);

  const notes = [...health.values()].filter((item) => item.reason !== "ok" && item.reason !== "disabled");
  const active = phase ? isActivePhase(phase.phase) : false;
  const elapsed = useElapsed(phase?.since, active, phase?.tier ? health.get(phase.tier) : undefined);
  if (!phase && notes.length === 0) return null;
  const worst = notes[0]?.reason ?? "ok";

  return (
    <div className={css.strip} data-testid="live-status-strip" data-phase={phase?.phase ?? null} data-health={worst}>
      {phase ? (
        <span className={css.phase} data-testid="live-phase">
          <span className={active ? css.liveDot : css.idleDot} aria-hidden />
          {PHASE_LABEL[phase.phase]}
          {phase.phase === "turn-ended" && phase.outcome ? ` · ${OUTCOME_LABEL[phase.outcome] ?? phase.outcome}` : null}
          {phase.toolName ? <span className={css.toolName}>{phase.toolName}</span> : null}
        </span>
      ) : null}
      {active && elapsed ? (
        <span
          className={elapsed.stale ? css.elapsedStale : css.elapsed}
          data-testid="live-elapsed"
          data-stale={elapsed.stale ? "1" : "0"}
          title={elapsed.stale ? "锚定该状态的通道已静默：时间仍在累计，但状态可能已经变化" : "自 harness 报告该状态起经过的时间，本地 1 Hz 计时"}
        >
          {elapsed.text}
        </span>
      ) : null}
      {phase?.tier ? (
        <span className={css.chip} data-testid="live-tier">
          {phase.tier}
          {phase.provision && phase.provision !== "native" ? ` · ${phase.provision}` : null}
        </span>
      ) : null}
      {phase?.phrase ? (
        // Spinner phrase: decoration carried at most once per transition.
        // Never parsed for meaning; the structured label is authoritative.
        <span className={css.phrase} data-testid="live-phrase">
          {phase.phrase}
        </span>
      ) : null}
      {notes.map((item) => (
        <HealthNote key={item.tier} health={item} />
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
