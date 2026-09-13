import type { Instance, UiStatus } from "../types/instance";
import { knowledgeValue } from "../types/command";

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
 * PTY-carried sessions have no structured conversation — except a promoted one,
 * which hydrates the native transcript and therefore renders like an agent.
 */
export function isGenericPty(instance: Instance): boolean {
  if (isPromoted(instance)) return false;
  return (
    instance.driver === "generic-pty" ||
    instance.driver === "shell-pty" ||
    instance.kind === "terminal"
  );
}

export function uiMode(instance: Instance): "structured-only" | "tty-attachable" {
  if (instance.kind === "terminal" || instance.driver === "shell-pty") return "tty-attachable";
  if (instance.driver === "claude-print") return "structured-only";
  if (instance.driver === "generic-pty" || instance.driver === "claude-pty") return "tty-attachable";
  const tty = instance.capabilities.capabilities["tty-attach"];
  return tty?.state === "supported" ? "tty-attachable" : "structured-only";
}

export function nativeShort(instance: Instance): string {
  const sid = instance.nativeRef.sessionId;
  if (sid.state !== "known") return "—";
  return sid.value.slice(0, 8);
}
