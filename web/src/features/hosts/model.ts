import type { Host } from "../../types/instance";
import type { Id } from "../../types/wire";

/** Proposal §4.6 carriers. ssh-dev / ssh-tunnel map onto ssh-stdio. */
export type Carrier = "ssh-stdio" | "outbound-wss" | "local";

export type HostCliAuth = "logged_in" | "logged_out" | "unknown";

export type HostCli = {
  kind: string;
  version: string;
  path: string;
  auth: HostCliAuth;
};

export type HostView = {
  id: Id;
  label: string;
  state: Host["state"];
  online: boolean;
  transport: Carrier;
  hostname?: string;
  port?: number;
  lastSeenAt?: string;
  rttMs?: number;
  agentVersion?: string;
  resources?: { cpuPct?: number; memPct?: number };
  cli: HostCli[];
  labels: string[];
  maxInstances: number;
  instanceCount: number;
  herdr?: { version: string; socket: string };
};

export type Placement =
  | { kind: "host"; hostId: Id }
  | { kind: "labels"; labels: string[] }
  | { kind: "any" };

export function carrierOf(mode: string): Carrier {
  if (mode === "local") return "local";
  if (mode === "outbound-wss") return "outbound-wss";
  return "ssh-stdio";
}

export function hostOnline(state: Host["state"]): boolean {
  return state === "online";
}

export function hostsMatching(hosts: HostView[], placement: Placement): HostView[] {
  if (placement.kind === "any") return hosts.filter((h) => h.online);
  if (placement.kind === "host") return hosts.filter((h) => h.id === placement.hostId);
  return hosts.filter((h) => h.online && placement.labels.every((label) => h.labels.includes(label)));
}
