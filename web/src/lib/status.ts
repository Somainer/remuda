import type { Instance, UiStatus } from "../types/instance";
import { knowledgeValue } from "../types/command";
import { canShowTerminal, hasStructuredSignal } from "../features/session/tty/gate";

/**
 * Chinese wording for the instance-level UI status. The single source the
 * header label and {@link StateDot} should converge on (C2 wired the header;
 * `StateDot` keeps its own identical map until a later batch touches it).
 */
export const UI_STATUS_LABEL: Record<UiStatus, string> = {
  blocked: "待处理",
  working: "运行中",
  starting: "启动中",
  idle: "空闲",
  exited: "已退出",
  unknown: "状态未知",
};

/** UI status dot = lifecycle × activity × connectivity (ui-spec §2.1). */
export function projectStatus(instance: Instance): UiStatus {
  if (instance.lifecycle === "unknown" || instance.lifecycle === "reconciling" || instance.connectivity !== "connected") {
    return "unknown";
  }
  const activity = knowledgeValue(instance.activity);
  if (activity === "waiting-interaction") return "blocked";
  if (instance.lifecycle === "requested" || instance.lifecycle === "preparing" || instance.lifecycle === "starting") {
    return "starting";
  }
  if (instance.lifecycle === "exited" || instance.lifecycle === "failed" || instance.lifecycle === "closing") {
    return "exited";
  }
  if (activity === "working" || activity === "draining") return "working";
  if (activity === "idle" && (instance.lifecycle === "ready" || instance.lifecycle === "running")) return "idle";
  if (instance.lifecycle === "running" || instance.lifecycle === "ready") return "working";
  return "unknown";
}

/**
 * A promoted terminal: a `shell-pty` instance whose PTY foreground is a known
 * agent CLI (D-025). Its driver is unchanged; what changes is that the session
 * has a real transcript and takes prompts.
 */
export function isPromoted(instance: Instance): boolean {
  return instance.mode === "promoted" && instance.kind !== "terminal";
}

/**
 * Render as a raw screen rather than a transcript.
 *
 * D-028 §1.0: an agent in a native PTY gets BOTH projections — it renders
 * the transcript whenever the session has a structured signal tier, and only
 * falls back to raw screen text without one.
 */
export function isGenericPty(instance: Instance): boolean {
  if (isPromoted(instance)) return false;
  if (instance.kind === "terminal") return true;
  const pty =
    instance.driver === "generic-pty" ||
    instance.driver === "shell-pty" ||
    instance.driver === "claude-pty";
  return pty && !hasStructuredSignal(instance);
}

export function uiMode(instance: Instance): "structured-only" | "tty-attachable" {
  // Carrier/capability, not driver name (D-028 §1.0 rule 4).
  return canShowTerminal(instance) ? "tty-attachable" : "structured-only";
}

export function nativeShort(instance: Instance): string {
  const sid = instance.nativeRef.sessionId;
  if (sid.state !== "known") return "—";
  return sid.value.slice(0, 8);
}
