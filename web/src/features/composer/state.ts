/**
 * Composer three-state machine (D-028 §6): 发送 / 排队 / 打断.
 *
 * The layout is decided purely by the session's *reported* capabilities
 * (`steer` / `queue` / `interrupt`, each `native | emulated | unknown`) and
 * the instance phase. Emulated stays visible — never dressed up as native —
 * and `unknown` renders an honest「尚未验证」note instead of a faked
 * enabled/disabled button.
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
  | { kind: "steer"; label: string; mode: Extract<PromptMode, "steer"> }
  | { kind: "queue"; label: string; mode: Extract<PromptMode, "queue"> };

export type QueueControl =
  | { available: false }
  | { available: true; holder: "remuda" | "native"; label: string };

export type InterruptControl =
  | { available: false }
  | { available: true; provision: CapabilityProvision; label: string; note: string | null };

export type ComposerState = {
  primary: PrimaryAction;
  queue: QueueControl;
  interrupt: InterruptControl;
  /** Working, but no native send-now: sending means cancelling the turn first. */
  interruptAndSend: boolean;
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

const STEER_LABEL: Record<string, string> = {
  claude: "发送",
  codex: "发送",
  grok: "发送",
  agy: "发送",
};

const STEER_HINT: Record<string, string> = {
  claude: "下一个工具边界插话",
  codex: "立即插话到当前 turn",
};

/**
 * @param kind    harness kind (for honest label copy)
 * @param phase   instance phase as projected by the UI
 * @param caps    the live session capability snapshot
 */
export function composerState(kind: string, phase: Phase, caps: CapabilitySnapshot): ComposerState {
  const c = triple(caps);
  const sendLabel = STEER_LABEL[kind] ?? "发送";

  if (phase === "blocked") {
    // D-022: while blocked, a send queues for delivery once the turn is ready.
    return {
      primary: { kind: "send", label: "发送", mode: "new-turn" },
      queue: { available: false },
      interrupt: supportedInterrupt(c.interrupt),
      interruptAndSend: false,
      note: null,
    };
  }

  if (phase !== "working") {
    return {
      primary: { kind: "send", label: "发送", mode: "new-turn" },
      queue: { available: false },
      interrupt: { available: false },
      interruptAndSend: false,
      note: null,
    };
  }

  const steer = provision(c.steer);
  const interrupt = supportedInterrupt(c.interrupt);

  // §6 mapping rule 2: the Remuda-held queue is ALWAYS available while
  // working. Only codex (native Tab) is marked as harness-native.
  const queueControl: QueueControl = isSupported(c.queue)
    ? provision(c.queue) === "native"
      ? { available: true, holder: "native", label: "排队（原生 Tab）" }
      : { available: true, holder: "remuda", label: "排队（Remuda 代持）" }
    : { available: false };

  if (steer === "native") {
    return {
      primary: { kind: "steer", label: sendLabel, mode: "steer" },
      queue: queueControl,
      interrupt,
      interruptAndSend: false,
      note: STEER_HINT[kind] ?? null,
    };
  }

  if (steer === "emulated") {
    return {
      // grok: there is no native send-now; the primary is the honest queue.
      primary: { kind: "queue", label: "排队", mode: "queue" },
      queue: queueControl,
      interrupt,
      interruptAndSend: true,
      note: "打断并发送会取消当前 turn",
    };
  }

  if (c.steer?.state === "unsupported") {
    return {
      primary: { kind: "queue", label: "排队", mode: "queue" },
      queue: queueControl,
      interrupt,
      interruptAndSend: false,
      note: "该 harness 不支持插话",
    };
  }

  // unknown: show「尚未验证」, never a fake-greyed or fake-native button.
  return {
    primary: { kind: "queue", label: "排队", mode: "queue" },
    queue: queueControl,
    interrupt,
    interruptAndSend: true,
    note: "steer 尚未验证：发送将打断当前 turn",
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
