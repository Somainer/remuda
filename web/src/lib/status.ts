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
  if (activity === "idle" && instance.lifecycle === "ready") return "idle";
  return "unknown";
}

export function uiMode(instance: Instance): "structured-only" | "tty-attachable" {
  if (instance.driver === "claude-print") return "structured-only";
  const tty = instance.capabilities.capabilities["tty-attach"];
  return tty?.state === "supported" ? "tty-attachable" : "structured-only";
}

export function nativeShort(instance: Instance): string {
  const sid = instance.nativeRef.sessionId;
  if (sid.state !== "known") return "—";
  return sid.value.slice(0, 8);
}
