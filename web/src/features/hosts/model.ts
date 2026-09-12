import type { Host } from "../../types/instance";
import type { Id } from "../../types/wire";

/** Proposal §4.6 carriers. ssh-dev / ssh-tunnel map onto ssh-stdio. */
export type Carrier = "ssh-stdio" | "outbound-wss" | "local";

export type HostCliAuth = "logged_in" | "logged_out" | "unknown" | "gateway-native" | "none";

export type HostCli = {
  kind: string;
  version?: string;
  path?: string;
  auth?: HostCliAuth;
  nativeGateway?: boolean;
  installed?: boolean;
};

/** Offline hosts older than this are hidden unless the operator shows stale. */
export const STALE_OFFLINE_MS = 30 * 60 * 1000;

export type HostSortable = {
  id: string;
  label?: string;
  state?: string;
  online?: boolean;
  lastSeenAt?: string;
  ssh?: Host["ssh"];
};

export type HostView = {
  id: Id;
  label: string;
  state: Host["state"];
  online: boolean;
  transport: Carrier;
  ssh?: Host["ssh"];
  lastError?: string;
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
  herdr?: { version?: string; socket?: string; path?: string };
  /** `auto` | `native` | `profile:<id>` */
  providerBinding: string;
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
  return state === "online" || state === "enrolled";
}

export function hostIsOnline(host: HostSortable): boolean {
  return host.online === true || host.state === "online" || host.state === "enrolled";
}

export function isStaleOffline(host: HostSortable, now = Date.now()): boolean {
  if (hostIsOnline(host) || host.ssh || host.state === "connecting") return false;
  if (!host.lastSeenAt) return true;
  const at = Date.parse(host.lastSeenAt);
  if (Number.isNaN(at)) return true;
  return now - at > STALE_OFFLINE_MS;
}

export function installedCli(cli: HostCli[] | undefined): HostCli[] {
  return (cli ?? []).filter((entry) => Boolean(entry.path || entry.version));
}

export function compactCliVersion(kind: string, version?: string): string {
  if (!version) return kind;
  let text = version.trim();
  const prefix = kind.toLowerCase();
  const lower = text.toLowerCase();
  if (lower.startsWith(`${prefix}-cli `)) text = text.slice(prefix.length + 5).trim();
  else if (lower.startsWith(`${prefix} `) || lower === prefix) {
    text = text.slice(prefix.length).trim();
  }
  text = text.replace(/\s*\([^)]*\)\s*$/, "").trim();
  return text ? `${kind} ${text}` : kind;
}

export function cliSummary(cli: HostCli[] | undefined): string {
  const installed = installedCli(cli);
  if (!installed.length) return "";
  return installed.map((entry) => compactCliVersion(entry.kind, entry.version)).join(" · ");
}

export function sortHostsOnlineFirst<T extends HostSortable>(hosts: T[], recentIds: string[] = []): T[] {
  const rank = new Map(recentIds.map((id, i) => [id, i]));
  return hosts.slice().sort((a, b) => {
    const ao = hostIsOnline(a) ? 0 : 1;
    const bo = hostIsOnline(b) ? 0 : 1;
    if (ao !== bo) return ao - bo;
    const ra = rank.get(a.id) ?? 99;
    const rb = rank.get(b.id) ?? 99;
    if (ra !== rb) return ra - rb;
    const at = a.lastSeenAt ?? "";
    const bt = b.lastSeenAt ?? "";
    if (at !== bt) return bt.localeCompare(at);
    return (a.label ?? a.id).localeCompare(b.label ?? b.id);
  });
}

export function hostsMatching(hosts: HostView[], placement: Placement): HostView[] {
  if (placement.kind === "any") return hosts.filter((h) => h.online);
  if (placement.kind === "host") return hosts.filter((h) => h.id === placement.hostId);
  return hosts.filter((h) => h.online && placement.labels.every((label) => h.labels.includes(label)));
}
