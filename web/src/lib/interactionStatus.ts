import type { Host } from "../types/instance";
import type { Interaction } from "../types/interaction";

export type InteractionUiState = "pending" | "answering" | "settled" | "expired" | "superseded" | "paused";

export const DEVICE_KEY = "runtime.device-id";

export function thisDeviceId(): string {
  try {
    const existing = localStorage.getItem(DEVICE_KEY);
    if (existing) return existing;
    const next = crypto.randomUUID();
    localStorage.setItem(DEVICE_KEY, next);
    return next;
  } catch {
    return "dev_local";
  }
}

function deadlinePassed(interaction: Interaction, now = Date.now()): boolean {
  if (interaction.deadline.state !== "known") return false;
  const t = Date.parse(interaction.deadline.value);
  return Number.isFinite(t) && t < now;
}

export function hostOnline(host: Host | undefined, connectivity?: string): boolean {
  if (connectivity && connectivity !== "connected") return false;
  if (!host) return true;
  return host.state === "online" || host.state === "enrolled";
}

/** UI interaction state (ui-spec §2.5). answering is local until journal interaction.answered. */
export function projectInteraction(
  interaction: Interaction,
  opts: { answering?: boolean; host?: Host; connectivity?: string; deviceId?: string },
): InteractionUiState {
  const deviceId = opts.deviceId ?? thisDeviceId();
  if (interaction.state === "expired" || deadlinePassed(interaction)) return "expired";
  if (opts.answering && (interaction.state === "pending" || interaction.state === "unknown")) return "answering";
  if (interaction.state === "invalidated") return "superseded";
  if (interaction.state === "answer-committed" || interaction.state === "resolved") {
    const actorDevice = interaction.answer.state === "known" ? interaction.answer.value.actor.deviceId : null;
    if (actorDevice && actorDevice !== deviceId) return "superseded";
    return "settled";
  }
  if (!hostOnline(opts.host, opts.connectivity)) return "paused";
  if (opts.answering) return "answering";
  return "pending";
}

export const INTERACTION_LABEL: Record<InteractionUiState, string> = {
  pending: "待处理",
  answering: "提交中",
  settled: "已处理",
  expired: "过期，未作用于新进程",
  superseded: "已在其它设备处理",
  paused: "主机离线，交互暂停",
};
