import type { Instance, Lifecycle } from "../types/instance";
import { knowledgeValue } from "../types/command";

/**
 * Human, correctly-toned reason an instance ENDED.
 *
 * The Hub and Node settle a terminal row with a machine code in
 * `instance.lastError` (`node-epoch-changed`, `model-mismatch…`,
 * `native-exit-code-3`, …) and/or structured `exit` evidence. List rows used
 * to print that wire string verbatim in failure red — so a demo Node restart
 * (a refresh, not a failure) read as 「进行中」 with `node-epoch-changed` in
 * red. This module is the single projection every session-status surface uses
 * instead (c-endreason, D-053 item 2).
 *
 * Invariants:
 *  - Returns `null` for a live row (anything not in a terminal lifecycle);
 *    callers keep their live status wording.
 *  - `tone === "failed"` is the ONLY thing a renderer may paint red.
 *    `interrupted` (the process/carrier/Node went away through no fault of
 *    the work) and `ended` (a normal or unattributable close) are neutral.
 *  - The machine code never appears in `label`. It survives verbatim in
 *    `detail`, the tooltip/details surface; `detail` is null for a clean exit.
 *  - An unrecognised code is not a failure: neutral 「已结束」 plus the raw
 *    text in `detail`. Unknown never renders red.
 *
 * Hub/Node codes are not changed here — this is a display rule over exactly
 * what the API returns.
 */

export type EndTone = "interrupted" | "failed" | "ended";

export type EndReason = {
  /** Short human Chinese sentence for the row body. */
  label: string;
  /** Raw machine code/text for a tooltip/details line; null for a clean exit. */
  detail: string | null;
  tone: EndTone;
};

export type EndReasonInput = Pick<Instance, "lifecycle" | "lastError" | "exit">;

/**
 * The `lastError` value the Hub writes when a Node reconnected under a new
 * epoch and no longer holds the instance. Exported so callers (e.g. the
 * session-page restart banner) can key behaviour off the same spelling.
 */
export const NODE_EPOCH_CHANGED = "node-epoch-changed";

const ENDED_LABEL = "已结束";

/**
 * Lifecycles the API serves for a finished session. `closing` projects to the
 * same exited UI status as the other two.
 */
const TERMINAL: ReadonlySet<Lifecycle> = new Set(["exited", "failed", "closing"]);

/** Exact-match codes: the wire spelling is the whole lastError line. */
const EXACT: Readonly<Record<string, { label: string; tone: EndTone }>> = {
  // The Node restarted (or otherwise stopped holding the session). The work
  // was interrupted, not failed — Resume keeps the conversation (D-026).
  [NODE_EPOCH_CHANGED]: { label: "Node 重启，会话已中断", tone: "interrupted" },
  // A stop reached a Node that does not know the instance.
  "node-lost-instance": { label: "Node 已丢失该会话，会话已中断", tone: "interrupted" },
  // Host connection gone while the row was settling; fate of the process
  // unknown, so never a claimed failure.
  "host-lost": { label: "主机失联，会话已中断", tone: "interrupted" },
  // Herdr-carried PTY: the carrier socket/server/process the pane lived in is
  // gone. Same human fact as a Node restart — the session was cut short.
  "herdr-carrier-lost": { label: "终端承载中断，会话已中断", tone: "interrupted" },
  "carrier-missing": { label: "终端承载中断，会话已中断", tone: "interrupted" },
  "carrier-shutdown": { label: "终端承载中断，会话已中断", tone: "interrupted" },
  "startup-orphan": { label: "终端承载中断，会话已中断", tone: "interrupted" },
  // PTY closed with no wait status.
  "native-exit-eof": { label: "终端已关闭，会话已中断", tone: "interrupted" },
  // A launch the Node never acknowledged; the slot is reaped.
  "create-never-acknowledged": { label: "会话启动未获确认", tone: "failed" },
  // Driver task ended/panicked or its journal commit failed — the harness
  // itself stopped the session, which IS a failure the owner should notice.
  "driver-task-exited": { label: "会话驱动已退出", tone: "failed" },
  "driver-task-panicked": { label: "会话驱动崩溃", tone: "failed" },
  "native-observation-commit-failed": { label: "会话状态记录失败", tone: "failed" },
  // Owner-initiated close and operator delete are ordinary endings.
  "explicit-close": { label: ENDED_LABEL, tone: "ended" },
  "native-exit": { label: ENDED_LABEL, tone: "ended" },
  "deleted-by-operator": { label: "会话已删除", tone: "ended" },
};

/** Classify one recognised code line; null when the table does not know it. */
function classifyCode(code: string): Omit<EndReason, "detail"> | null {
  const exact = EXACT[code];
  if (exact) return exact;
  // model_pin_mismatch is journaled as a warning today, but older/other
  // channels surface the suffixed wire form (`model-mismatch: requested …
  // observed …`); both spellings name the same stop. Recorded fact, red.
  if (code === "model-mismatch" || code.startsWith("model-mismatch:") || code.startsWith("model-mismatch ")) {
    return { label: "模型与请求不一致，已停止", tone: "failed" };
  }
  let m = /^native-exit-code-(\d+)$/.exec(code);
  if (m) {
    const n = Number(m[1]);
    return n === 0
      ? { label: ENDED_LABEL, tone: "ended" }
      : { label: `会话异常退出（exit ${n}）`, tone: "failed" };
  }
  m = /^native-exit-signal-(.+)$/.exec(code);
  if (m) {
    return { label: `进程被终止（${m[1]}），会话已中断`, tone: "interrupted" };
  }
  return null;
}

/** Structured exit evidence, when the API served it. */
function classifyExit(exit: Instance["exit"]): Omit<EndReason, "detail"> | null {
  const value = knowledgeValue(exit);
  if (!value) return null;
  if (typeof value.signal === "string" && value.signal.trim()) {
    return { label: `进程被终止（${value.signal.trim()}），会话已中断`, tone: "interrupted" };
  }
  if (typeof value.code === "number") {
    return value.code === 0
      ? { label: ENDED_LABEL, tone: "ended" }
      : { label: `会话异常退出（exit ${value.code}）`, tone: "failed" };
  }
  return null;
}

/**
 * Project a terminal instance to its human end reason. `null` means the
 * instance is not ended and the caller must not render an end sentence.
 */
export function endReason(instance: EndReasonInput): EndReason | null {
  if (!TERMINAL.has(instance.lifecycle)) return null;
  // Codes are a single line; a multi-line free-text error keeps its first line
  // for classification and its full (trimmed) text for the tooltip.
  const raw = instance.lastError?.trim() ? instance.lastError!.trim() : null;
  const code = raw ? (raw.split(/\r?\n/, 1)[0]?.trim() ?? "") : "";
  if (code) {
    const known = classifyCode(code);
    if (known) return { ...known, detail: raw };
  }
  const fromExit = classifyExit(instance.exit);
  if (fromExit) return { ...fromExit, detail: raw };
  // Unknown wire text (or no text at all): a neutral ending. The raw string,
  // when one exists, is available in the tooltip but never painted red.
  return { label: ENDED_LABEL, detail: raw, tone: "ended" };
}
