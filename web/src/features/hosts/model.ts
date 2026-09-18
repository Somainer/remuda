import type { Host, TuiMode } from "../../types/instance";
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
  resources?: { cpuPct?: number; memPct?: number; sampledAt?: string };
  cli: HostCli[];
  labels: string[];
  maxInstances: number;
  instanceCount: number;
  herdr?: { version?: string; socket?: string; path?: string };
  /** `auto` | `native` | `profile:<id>` */
  providerBinding: string;
  /** Per-host default extra CLI args, applied when a create omits `args`. */
  defaultLaunchArgs?: string[];
  defaultTui?: TuiMode;
  /** Per-host default claude executable. Validated by the Node, not the Hub. */
  claudeBinaryPath?: string;
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

/** `cli[]` kind of the vendor Computer Use presence row (D-045 §3.4). */
export const COMPUTER_USE_KIND = "computer-use";

/**
 * What the host said about desktop control.
 *
 * Three states, and the third is the one that matters: a Node that predates
 * this row reports nothing, which is **not** a claim that the host cannot do
 * it. Rendering "not reported" as "unsupported" would invent a fact, so the
 * distinction is carried in the type rather than flattened to a boolean.
 */
export type ComputerUseState =
  | { reported: false }
  | { reported: true; installed: false }
  | { reported: true; installed: true; version?: string; path?: string };

export function computerUseState(cli: HostCli[] | undefined): ComputerUseState {
  const entry = (cli ?? []).find((item) => item.kind === COMPUTER_USE_KIND);
  if (!entry) return { reported: false };
  if (entry.installed === false) return { reported: true, installed: false };
  return { reported: true, installed: true, version: entry.version, path: entry.path };
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
