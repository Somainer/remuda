import { formatListTime } from "../../lib/format";

/**
 * Per-session token/context rollup, Hub-computed from the journal's usage
 * observations (context-usage-1). Every field except `turns` is `null` until
 * at least one usage observation has reported that channel — Codex/Grok
 * adapters report only subsets, and the UI must render `—`, never a fake 0.
 */
export type UsageRollup = {
  /** Last turn's fresh input + cache read + cache creation. */
  contextUsedTokens: number | null;
  /** Context window ([1m] tag / model catalog / harness kind). */
  contextWindowTokens: number | null;
  /** 0..100; null until both sides are known. */
  contextPct: number | null;
  /** Session sum of fresh (uncached) input. */
  sessionInputTokens: number | null;
  /** Session sum of output. */
  sessionOutputTokens: number | null;
  /** Session sum of prompt-cache reads. */
  cacheReadTokens: number | null;
  /** Session sum of prompt-cache creations (writes). */
  cacheCreationTokens: number | null;
  /** Folded usage observations (one per model turn for Claude). */
  turns: number;
  /** Fresh input tokens observed in the last 60 s. */
  tpmIn60s: number | null;
  /** Output tokens observed in the last 60 s. */
  tpmOut60s: number | null;
  /** Average per-minute input rate over the last 5 min. */
  tpmIn5m: number | null;
  /** Average per-minute output rate over the last 5 min. */
  tpmOut5m: number | null;
  /** Observed-at of the most recent usage event. */
  lastTurnAt: string | null;
};

type RawRollup = Partial<Record<keyof UsageRollup, unknown>>;

/** Numbers come over the wire as JSON integers (the OpenAPI shape); anything
 *  else (string, missing, NaN) is treated as unknown, never coerced to 0. */
function asCount(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? Math.trunc(value)
    : null;
}

/** Accept the wire object in any looseness; one bad field never hides all. */
export function coerceUsageRollup(raw: unknown): UsageRollup | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as RawRollup;
  return {
    contextUsedTokens: asCount(r.contextUsedTokens),
    contextWindowTokens: asCount(r.contextWindowTokens),
    contextPct: asCount(r.contextPct),
    sessionInputTokens: asCount(r.sessionInputTokens),
    sessionOutputTokens: asCount(r.sessionOutputTokens),
    cacheReadTokens: asCount(r.cacheReadTokens),
    cacheCreationTokens: asCount(r.cacheCreationTokens),
    turns: asCount(r.turns) ?? 0,
    tpmIn60s: asCount(r.tpmIn60s),
    tpmOut60s: asCount(r.tpmOut60s),
    tpmIn5m: asCount(r.tpmIn5m),
    tpmOut5m: asCount(r.tpmOut5m),
    lastTurnAt: typeof r.lastTurnAt === "string" ? r.lastTurnAt : null,
  };
}

/** Compact token count: `66.0k`, `1.2M`; exact integers on hover title. */
export function formatTokenCount(value: number | null): string {
  if (value == null) return "—";
  if (value < 1000) return String(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(1)}k`;
  return `${(value / 1_000_000).toFixed(2)}M`;
}

/** Exact group-separated integer for tooltips. */
export function exactTokenCount(value: number | null): string {
  return value == null ? "" : value.toLocaleString("en-US");
}

export type UsageCell = {
  label: string;
  value: string;
  exact: string | null;
  /** Set when the value is unknown: names the channel that would supply it. */
  missingChannel: string | null;
};

const MISSING: Record<string, string> = {
  context: "需要 harness 在 usage 观察中上报本轮的 input / cacheRead / cacheCreation token",
  window: "需要 model 目录、[1m] 后缀或 harness 默认窗口提供上下文长度",
  input: "需要 harness 在 usage 观察中上报 inputTokens（非缓存输入）",
  output: "需要 harness 在 usage 观察中上报 outputTokens",
  cacheRead: "需要 harness 在 usage 观察中上报 cacheReadTokens（Claude 上报，部分 harness 暂不支持）",
  cacheCreation:
    "需要 harness 在 usage 观察中上报 cacheWriteTokens（Claude 上报，部分 harness 暂不支持）",
  tpmIn: "近 60 秒内没有任何一回合上报过 inputTokens",
  tpmOut: "窗口内没有任何一回合上报过 outputTokens",
  lastTurn: "本会话还没有任何 usage 观察",
};

export function contextHeadline(rollup: UsageRollup): {
  text: string;
  missing: string | null;
} {
  if (rollup.contextUsedTokens == null || rollup.contextWindowTokens == null) {
    return {
      text: `${formatTokenCount(rollup.contextUsedTokens)}/${formatTokenCount(
        rollup.contextWindowTokens,
      )} (—%)`,
      missing: rollup.contextUsedTokens == null ? MISSING.context : MISSING.window,
    };
  }
  const pct =
    rollup.contextPct ??
    Math.round((rollup.contextUsedTokens / rollup.contextWindowTokens) * 100);
  return {
    text: `${formatTokenCount(rollup.contextUsedTokens)}/${formatTokenCount(
      rollup.contextWindowTokens,
    )} (${pct}%)`,
    missing: null,
  };
}

export function sessionCells(rollup: UsageRollup): UsageCell[] {
  const make = (
    label: string,
    value: number | null,
    channel: string,
  ): UsageCell => ({
    label,
    value: formatTokenCount(value),
    exact: value == null ? null : exactTokenCount(value),
    missingChannel: value == null ? channel : null,
  });
  return [
    make("入", rollup.sessionInputTokens, MISSING.input),
    make("出", rollup.sessionOutputTokens, MISSING.output),
    make("缓存读", rollup.cacheReadTokens, MISSING.cacheRead),
    make("缓存写", rollup.cacheCreationTokens, MISSING.cacheCreation),
  ];
}

export type TpmCell = {
  label: string;
  in: UsageCell;
  out: UsageCell;
};

export function tpmCells(rollup: UsageRollup): TpmCell[] {
  return [
    {
      label: "60 秒",
      in: {
        label: "入",
        value: formatTokenCount(rollup.tpmIn60s),
        exact: rollup.tpmIn60s == null ? null : `${exactTokenCount(rollup.tpmIn60s)}/min`,
        missingChannel: rollup.tpmIn60s == null ? MISSING.tpmIn : null,
      },
      out: {
        label: "出",
        value: formatTokenCount(rollup.tpmOut60s),
        exact: rollup.tpmOut60s == null ? null : `${exactTokenCount(rollup.tpmOut60s)}/min`,
        missingChannel: rollup.tpmOut60s == null ? MISSING.tpmOut : null,
      },
    },
    {
      label: "5 分钟",
      in: {
        label: "入",
        value: formatTokenCount(rollup.tpmIn5m),
        exact: rollup.tpmIn5m == null ? null : `${exactTokenCount(rollup.tpmIn5m)}/min 均值`,
        missingChannel: rollup.tpmIn5m == null ? MISSING.tpmIn : null,
      },
      out: {
        label: "出",
        value: formatTokenCount(rollup.tpmOut5m),
        exact: rollup.tpmOut5m == null ? null : `${exactTokenCount(rollup.tpmOut5m)}/min 均值`,
        missingChannel: rollup.tpmOut5m == null ? MISSING.tpmOut : null,
      },
    },
  ];
}

export function lastTurnLabel(rollup: UsageRollup, nowMs: number): UsageCell {
  const value = rollup.lastTurnAt ? formatListTime(rollup.lastTurnAt, nowMs) : "—";
  return {
    label: "最近一回合",
    value,
    exact: rollup.lastTurnAt,
    missingChannel: rollup.lastTurnAt == null ? MISSING.lastTurn : null,
  };
}

export { MISSING as USAGE_MISSING_CHANNELS };
