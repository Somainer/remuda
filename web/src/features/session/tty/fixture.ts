import type { Instance } from "../../../types/instance";
import type { Capability, CapabilityName, CapabilitySnapshot } from "../../../types/nativeRef";
import { known, unknownKnowledge, type Id } from "../../../types/wire";
import { utf8Bytes } from "./ids";
import { devTtyEnabled } from "./gate";

/**
 * Recorded ANSI frame for the TTY lab.
 * Source: /tmp/hh-spike-pty/final-screen.txt (hh-spike-pty Claude TUI dump, 2026-09)
 * plus CSI home/clear so xterm paints the distinctive resume banner.
 */
export const TTY_LAB_INSTANCE_ID = "ins_01993ab0-0000-7000-8000-00000000aa01" as Id;
export const TTY_LAB_STREAM_ID = "tty_01993ab0-0000-7000-8000-00000000aa02" as Id;
export const TTY_LAB_EPOCH_ID = "epoch_01993ab0-0000-7000-8000-00000000aa03" as Id;
export const TTY_LAB_LEASE_ID = "obj_01993ab0-0000-7000-8000-00000000aa04" as Id;
export const TTY_LAB_JOURNAL_ID = "obj_01993ab0-0000-7000-8000-00000000aa05" as Id;
export const TTY_LAB_HOST_ID = "hst_01993ab0-0000-7000-8000-00000000aa06" as Id;
export const TTY_LAB_WORKSPACE_ID = "wsp_01993ab0-0000-7000-8000-00000000aa07" as Id;

export const ANSI_FIXTURE_TEXT =
  "\u001b[?1049h\u001b[2J\u001b[H\u001b[1;36mclaude\u001b[0m  pty lab\r\n" +
  "Resume this session with:\r\n" +
  "claude --resume 477c322e-9208-49e1-b5d6-8f79df71cf7f\r\n";

export const ANSI_FIXTURE_BYTES = utf8Bytes(ANSI_FIXTURE_TEXT);

const NAMES: CapabilityName[] = [
  "resume",
  "steer",
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

function ptyCapabilities(): CapabilitySnapshot {
  const capabilities = {} as Record<CapabilityName, Capability>;
  for (const name of NAMES) capabilities[name] = cap("unknown", "unverified");
  capabilities["tty-attach"] = cap("supported", "herdr-observe");
  capabilities.resume = cap("supported", "resume");
  capabilities.artifact = cap("supported", "native-tui");
  return {
    id: "obj_01993ab0-0000-7000-8000-00000000aa08" as Id,
    driverKind: "claude-pty",
    adapterVersion: "0.1.0",
    binaryVersion: "2.1.268",
    binaryDigest: `sha256:${"cd".repeat(32)}`,
    nativeProtocolVersion: unknownKnowledge("not-negotiated"),
    settingsRevision: "1",
    providerProfileRevision: "1",
    capabilities,
  };
}

export function ttyLabInstance(): Instance {
  const ts = "2026-09-12T00:00:00.000Z";
  return {
    id: TTY_LAB_INSTANCE_ID,
    revision: "1",
    createdAt: ts,
    updatedAt: ts,
    hostId: TTY_LAB_HOST_ID,
    workspaceId: TTY_LAB_WORKSPACE_ID,
    kind: "claude",
    driver: "claude-pty",
    lifecycle: "ready",
    activity: known("idle"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {
      hostId: TTY_LAB_HOST_ID,
      nativeStoreId: "obj_01993ab0-0000-7000-8000-00000000aa09" as Id,
      kind: "claude",
      sessionId: known("477c322e-9208-49e1-b5d6-8f79df71cf7f"),
      transcript: unknownKnowledge("not-exported"),
      claude: { sessionId: "477c322e-9208-49e1-b5d6-8f79df71cf7f" },
    },
    processRef: {
      processGeneration: "1",
      processIdentity: unknownKnowledge("lab"),
      connectionEpoch: TTY_LAB_EPOCH_ID,
    },
    specRevision: "1",
    launchId: known("launch_01993ab0-0000-7000-8000-00000000aa0a" as Id),
    capabilities: ptyCapabilities(),
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: TTY_LAB_JOURNAL_ID,
    durableSeq: "0",
    exit: { state: "not-applicable" },
  };
}

export function isTtyLabFixtureId(instanceId: string): boolean {
  return instanceId === TTY_LAB_INSTANCE_ID;
}

export function resolveTtyLabInstance(instanceId: string): Instance | undefined {
  if (!devTtyEnabled() || instanceId !== TTY_LAB_INSTANCE_ID) return undefined;
  return ttyLabInstance();
}
