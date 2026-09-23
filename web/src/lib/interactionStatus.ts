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

/**
 * Whether the interaction is settled on THIS device by the state branches
 * projectInteraction resolves BEFORE looking at host connectivity, in the
 * same evaluation order:
 *  - expired state or a passed deadline wins (it projects "expired", a
 *    visible 已离队 row, so this is never "settled");
 *  - answer-committed/resolved by another device projects "superseded";
 *  - answer-committed/resolved with no actor or by this device projects
 *    "settled" and renders nowhere.
 *
 * Lets list derivation drop no-render rows before joining instances/hosts
 * (c-inboxperf) without reimplementing the order-sensitive rule.
 */
export function settledOnThisDevice(
  interaction: Interaction,
  deviceId: string = thisDeviceId(),
): boolean {
  if (interaction.state === "expired" || deadlinePassed(interaction)) return false;
  if (interaction.state === "answer-committed" || interaction.state === "resolved") {
    const actorDevice =
      interaction.answer.state === "known" ? interaction.answer.value.actor.deviceId : null;
    if (actorDevice && actorDevice !== deviceId) return false;
    return true;
  }
  return false;
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

/**
 * Whether the native side confirmed it dropped this request.
 *
 * `answer-committed` only says Remuda durably recorded an answer. Until the
 * harness clears the request, the turn has not moved on — that gap is the
 * "回答已提交，等待处理" row in P0-3.
 */
export function nativeCleared(interaction: Interaction): boolean {
  return interaction.resolution.state === "known" && interaction.resolution.value.reason === "native-cleared";
}

/**
 * An answer is recorded but the native side has not cleared the request.
 *
 * The form is very likely still on screen in this state, which is exactly when
 * a user tries to submit a second time.
 */
export function answerPendingNative(interaction: Interaction): boolean {
  const committed = interaction.state === "answer-committed" || interaction.state === "resolved";
  return committed && !nativeCleared(interaction);
}

/**
 * Whether a human may still submit an answer.
 *
 * The single guard for the double-submit case P0-3 calls out (已答请求不能二次
 * 提交). Deliberately strict: `pending` is the only submittable projection, so
 * an in-flight local submit, an answer from another device, an expired
 * deadline, an invalidated request and an offline host all block it. Callers
 * should disable their submit control on `false` rather than reproduce any
 * part of this rule.
 */
export function canSubmitAnswer(
  interaction: Interaction,
  opts: { answering?: boolean; host?: Host; connectivity?: string; deviceId?: string } = {},
): boolean {
  if (!interaction.answerable) return false;
  return projectInteraction(interaction, opts) === "pending";
}
