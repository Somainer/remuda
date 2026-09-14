import type {
  Capability,
  CapabilityName,
  CapabilityProvision,
  CapabilitySnapshot,
  DriverKind,
  NativeRef,
  SignalTier,
} from "../types/nativeRef";
import { unknownKnowledge } from "../types/wire";
import { digestPlaceholder, id } from "./ids";

const NAMES: CapabilityName[] = [
  "resume",
  "steer",
  "queue",
  "interrupt",
  "model-switch",
  "fork",
  "structured-workflow",
  "artifact",
  "tty-attach",
  "hooks",
  "interactive-approval",
  "question",
  "plan-review",
  "elicitation",
  "live-attach",
  "completion-native-turn",
  "completion-task",
];

function cap(state: Capability["state"], reason: string): Capability {
  return { state, scope: [], reasonCode: reason, prerequisites: [], evidence: [] };
}

/** Present an emulated capability as emulated. D-028 §6 requires it be user-visible: an
 *  emulated queue lives in Remuda's ledger, a native one inside the harness. */
export function provisionOf(capability: Capability | undefined): CapabilityProvision {
  return capability?.provision ?? "unknown";
}

/**
 * Capabilities a signal tier structurally provides; D-028 §4.3.
 *
 * Mirrors `capability_set_with_runtime` in `remuda-driver`. A hook socket is
 * the only tier that can block the agent and return a verdict, which is what
 * `interactive-approval` means. OSC and Screen prove neither identity nor turn
 * boundaries, so they add nothing — that is why they rank lowest.
 */
const TIER_CAPABILITIES: Record<SignalTier, CapabilityName[]> = {
  hook: ["resume", "hooks", "structured-workflow", "completion-native-turn", "interactive-approval"],
  file: ["resume", "structured-workflow", "completion-native-turn"],
  osc: [],
  screen: [],
  none: [],
};

/**
 * Layer a session's runtime capability report over a static snapshot; §4.3.
 *
 * The static snapshot keys off `driverKind`, so a promoted shell-pty session
 * cannot express what it actually gained by becoming an agent. Runtime values
 * win per name whatever their state — a session reporting `steer` as
 * unavailable says so rather than inheriting a matrix `supported` — and a name
 * with no runtime entry keeps its static value, because "not reported" is not
 * evidence of absence.
 */
export function withRuntimeCapabilities(
  snapshot: CapabilitySnapshot,
  nativeRef: Pick<NativeRef, "signalTier" | "capabilities"> | undefined,
): CapabilitySnapshot {
  if (!nativeRef?.signalTier && !nativeRef?.capabilities?.length) return snapshot;
  const capabilities = { ...snapshot.capabilities };
  if (nativeRef.signalTier) {
    for (const name of TIER_CAPABILITIES[nativeRef.signalTier] ?? []) {
      capabilities[name] = {
        ...cap("supported", `signal-tier-${nativeRef.signalTier}`),
        provision: "native",
      };
    }
  }
  // Explicit entries are more specific than the tier default, so they win.
  for (const entry of nativeRef.capabilities ?? []) {
    capabilities[entry.name] = { ...cap(entry.state, entry.reasonCode), provision: entry.provision };
  }
  return { ...snapshot, capabilities };
}

export function ptyCapabilities(driverKind: DriverKind = "generic-pty"): CapabilitySnapshot {
  const snapshot = printCapabilities();
  snapshot.driverKind = driverKind;
  snapshot.capabilities["tty-attach"] = cap("supported", "herdr-pty");
  snapshot.capabilities.artifact = cap("supported", "pty-scrollback");
  return snapshot;
}

/**
 * Static fallback matrix for an agent harness running in a Remuda-owned PTY
 * (D-028 §6 measured results). Runtime reports layered by
 * {@link withRuntimeCapabilities} win per name; this is what a fresh
 * shell-pty session is honest about before the Node reports live.
 *
 * claude: Enter steers at the next tool boundary (native); after-turn queue
 * is Remuda-held; Esc interrupts natively.
 * codex: Enter steers immediately, Tab queues natively, Esc interrupts.
 * grok: no native send-now (queue primary, send = cancel+resend); queue is
 * Remuda-held; interrupt needs the emulated double-Ctrl+C sequence.
 * agy: unmeasured — every one of the three stays `unknown`.
 */
const AGENT_PTY_MATRIX: Record<
  string,
  { steer: CapabilityProvision; queue: CapabilityProvision; interrupt: CapabilityProvision }
> = {
  claude: { steer: "native", queue: "emulated", interrupt: "native" },
  codex: { steer: "native", queue: "native", interrupt: "native" },
  grok: { steer: "unknown", queue: "emulated", interrupt: "emulated" },
  agy: { steer: "unknown", queue: "unknown", interrupt: "unknown" },
};

function provisioned(state: Capability["state"], reason: string, provision: CapabilityProvision): Capability {
  return { ...cap(state, reason), provision };
}

export function agentPtyCapabilities(
  kind: string,
  driverKind: DriverKind = "shell-pty",
): CapabilitySnapshot {
  const snapshot = ptyCapabilities(driverKind);
  const row = AGENT_PTY_MATRIX[kind];
  if (!row) return snapshot;
  for (const name of ["steer", "queue", "interrupt"] as const) {
    const p = row[name];
    snapshot.capabilities[name] =
      p === "unknown"
        ? provisioned("unknown", `${kind}-${name}-unverified`, "unknown")
        : provisioned("supported", `${kind}-${name}-${p}`, p);
  }
  // Promoted/launched agents hydrate a transcript tail; structured is a real
  // projection alongside the terminal one (D-028 §1.0).
  snapshot.capabilities["structured-workflow"] = provisioned(
    "supported",
    `${kind}-transcript-tail`,
    "native",
  );
  return snapshot;
}

export function printCapabilities(): CapabilitySnapshot {
  const capabilities = {} as Record<CapabilityName, Capability>;
  for (const name of NAMES) capabilities[name] = cap("unknown", "unverified");
  capabilities.resume = cap("supported", "resume");
  capabilities.hooks = cap("supported", "hooks");
  capabilities["structured-workflow"] = cap("supported", "workflow-tool");
  capabilities["interactive-approval"] = cap("supported", "host-control");
  capabilities.question = cap("supported", "ask-user");
  capabilities["tty-attach"] = cap("unsupported", "print-has-no-tui");
  capabilities.artifact = cap("unsupported", "print-no-artifact");
  capabilities["completion-native-turn"] = cap("supported", "result");
  return {
    id: id("obj_"),
    driverKind: "claude-print" satisfies DriverKind,
    adapterVersion: "0.1.0",
    binaryVersion: "2.1.268",
    binaryDigest: digestPlaceholder(),
    nativeProtocolVersion: unknownKnowledge("not-negotiated"),
    settingsRevision: "1",
    providerProfileRevision: "1",
    capabilities,
  };
}
