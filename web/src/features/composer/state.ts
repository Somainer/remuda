/**
 * Composer state machine (c-steer, formerly D-028 §6): 排队 / 插队 / 打断.
 *
 * Working semantics after c-steer:
 *
 * - **Enter / 排队 button** holds the message until the turn ends
 *   (`排队中 · 第 n 条 · 回车后送出`, removable). When the harness owns a
 *   native queue (codex Tab) the hold is posted immediately with `mode:queue`
 *   and the chip is a ledger mirror; otherwise Remuda holds it client-side and
 *   posts `new-turn` on the working→idle transition.
 * - **Cmd/Ctrl+Enter / 插队 button** interrupts the running turn (Esc through
 *   the driver's own key path) and delivers this message first, ahead of every
 *   held one. The Node journals origin+reason; provision is reported honestly
 *   (`native` Esc, `emulated` cancel sequence, `unknown` 尚未验证).
 * - **打断** ends the turn with no message.
 * - **blocked** (a question/approval/elicitation/plan review is pending) is
 *   NOT "working": Enter holds the message with「待回答后送出」and it flushes
 *   when the interaction resolves; nothing is ever stuck in 排队中 without a
 *   visible reason.
 */
import type { Capability, CapabilityProvision, CapabilitySnapshot } from "../../types/nativeRef";
import type { PromptMode } from "../../types/generated";

export type Phase = "idle" | "working" | "blocked" | "exited";

export type CapTriple = {
  steer: Capability | undefined;
  queue: Capability | undefined;
  interrupt: Capability | undefined;
};

export type PrimaryAction =
  | { kind: "send"; label: string; mode: Extract<PromptMode, "new-turn"> }
  | {
      kind: "queue";
      label: string;
      mode: Extract<PromptMode, "queue">;
      /** Who holds the queued message until the turn ends. */
      holder: "remuda" | "native";
      /** Why the message waits; rendered on the pending transcript row. */
      waitNote: string;
    };

export type QueueControl =
  | { available: false }
  | { available: true; holder: "remuda" | "native"; label: string };

export type InterruptControl =
  | { available: false }
  | { available: true; provision: CapabilityProvision; label: string; note: string | null };

export type ComposerState = {
  primary: PrimaryAction;
  /** Secondary queue action metadata (also carried by the primary while busy). */
  queue: QueueControl;
  /** Plain 打断 (Esc), no message. */
  interrupt: InterruptControl;
  /** 插队: interrupt the turn AND deliver this message first (c-steer). */
  steer: InterruptControl;
  /** Honest caveat rendered next to the controls (emulation / unverified). */
  note: string | null;
};

export function provision(cap: Capability | undefined): CapabilityProvision {
  return cap?.provision ?? "unknown";
}

export function isSupported(cap: Capability | undefined): boolean {
  // Absent reads as unknown (still usable with a caveat); only an explicit
  // unsupported removes the control.
  return cap?.state !== "unsupported";
}

export function triple(caps: CapabilitySnapshot): CapTriple {
  return {
    steer: caps.capabilities.steer,
    queue: caps.capabilities.queue,
    interrupt: caps.capabilities.interrupt,
  };
}

/**
 * @param kind    harness kind (for honest label copy)
 * @param phase   instance phase as projected by the UI
 * @param caps    the live session capability snapshot
 */
export function composerState(_kind: string, phase: Phase, caps: CapabilitySnapshot): ComposerState {
  const c = triple(caps);

  if (phase !== "working" && phase !== "blocked") {
    return {
      primary: { kind: "send", label: "发送", mode: "new-turn" },
      queue: { available: false },
      interrupt: { available: false },
      steer: { available: false },
      note: null,
    };
  }

  const interrupt = supportedInterrupt(c.interrupt);
  if (phase === "blocked") {
    // A pending interaction is a human turn, not a working turn. The message
    // waits for the interaction to resolve, then goes out as a normal prompt.
    return {
      primary: {
        kind: "queue",
        label: "排队",
        mode: "queue",
        holder: "remuda",
        waitNote: "待回答后送出",
      },
      queue: { available: false },
      interrupt,
      // 插队 at a dialog would send Esc into the question — never.
      steer: { available: false },
      note: "问题处理中，消息将在回答后送出",
    };
  }

  // Working: Enter queues (Remuda-held, or the harness-native queue when one
  // is measured — codex Tab). 插队 is the emulated interrupt-and-send and
  // follows the interrupt key's reported provision.
  const nativeQueue = isSupported(c.queue) && provision(c.queue) === "native";
  const queueControl: QueueControl = isSupported(c.queue)
    ? nativeQueue
      ? { available: true, holder: "native", label: "排队（原生 Tab）" }
      : { available: true, holder: "remuda", label: "排队（Remuda 代持）" }
    : { available: true, holder: "remuda", label: "排队（Remuda 代持）" };
  const holder = nativeQueue ? "native" : "remuda";
  return {
    primary: {
      kind: "queue",
      label: "排队",
      mode: "queue",
      holder,
      waitNote: "回合结束后送出",
    },
    queue: queueControl,
    interrupt,
    steer: interrupt.available
      ? {
          available: true,
          provision: interrupt.provision,
          label: "插队",
          note: interrupt.note,
        }
      : { available: false },
    note:
      holder === "remuda"
        ? "Enter 排队（Remuda 代持）· ⌘/Ctrl+Enter 插队"
        : "Enter 原生排队（Tab）· ⌘/Ctrl+Enter 插队",
  };
}

function supportedInterrupt(cap: Capability | undefined): InterruptControl {
  if (!isSupported(cap)) return { available: false };
  const p = provision(cap);
  if (p === "native") return { available: true, provision: p, label: "打断", note: null };
  if (p === "emulated") {
    return { available: true, provision: p, label: "打断", note: "Remuda 代发取消序列" };
  }
  return { available: true, provision: p, label: "打断", note: "尚未验证" };
}
