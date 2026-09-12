import type { Capability, CapabilityName, CapabilitySnapshot, DriverKind } from "../types/nativeRef";
import { unknownKnowledge } from "../types/wire";
import { digestPlaceholder, id } from "./ids";

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
