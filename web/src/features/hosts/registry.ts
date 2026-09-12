import { useSyncExternalStore } from "react";
import type { Host, Instance } from "../../types/instance";
import { HOST_FIXTURES } from "./fixtures";
import { carrierOf, hostOnline, sortHostsOnlineFirst, type HostCli, type HostCliAuth, type HostView } from "./model";

const useFixtures = import.meta.env.VITE_MOCK === "1";

function mapCli(cli: Host["cli"]): HostCli[] {
  return (cli ?? []).map((entry) => ({
    kind: entry.kind,
    version: entry.version,
    path: entry.path,
    auth: (entry.auth ?? "unknown") as HostCliAuth,
    nativeGateway: entry.nativeGateway,
    installed: entry.installed,
  }));
}

type Listener = () => void;

class HostRegistry {
  private patches = new Map<string, Partial<HostView>>();
  private enrolled: HostView[] = [];
  private listeners = new Set<Listener>();
  private version = 0;

  subscribe = (listener: Listener) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  snapshot = () => this.version;

  private emit() {
    this.version += 1;
    for (const listener of this.listeners) listener();
  }

  patch(hostId: string, patch: Partial<HostView>) {
    this.patches.set(hostId, { ...this.patches.get(hostId), ...patch });
    this.emit();
  }

  enroll(host: HostView) {
    this.enrolled = [host, ...this.enrolled.filter((h) => h.id !== host.id && h.label !== host.label)];
    this.emit();
  }

  views(hubHosts: Host[], instances: Instance[]): HostView[] {
    const usedLabels = new Set<string>();
    const out: HostView[] = [];
    for (const host of hubHosts) {
      const extra = useFixtures ? HOST_FIXTURES.find((f) => f.label === host.label) : undefined;
      usedLabels.add(host.label);
      const liveCli = mapCli(host.cli);
      const base: HostView = extra
        ? {
            ...extra,
            id: host.id,
            state: host.state,
            online: host.online ?? hostOnline(host.state),
            lastSeenAt: host.lastSeenAt ?? extra.lastSeenAt,
            cli: liveCli.length ? liveCli : extra.cli,
            agentVersion: host.nodeVersion ?? extra.agentVersion,
            resources: host.resources ?? extra.resources,
            labels: host.labels?.length ? host.labels : extra.labels,
            maxInstances: host.maxInstances ?? extra.maxInstances,
            hostname: host.hostname ?? extra.hostname,
            providerBinding: host.providerBinding ?? extra.providerBinding ?? "auto",
          }
        : {
            id: host.id,
            label: host.label,
            state: host.state,
            online: host.online ?? hostOnline(host.state),
            transport: carrierOf(host.transport.mode),
            hostname: host.hostname,
            port: host.port,
            lastSeenAt: host.lastSeenAt,
            agentVersion: host.nodeVersion,
            resources: host.resources,
            cli: liveCli,
            labels: host.labels ?? [],
            maxInstances: host.maxInstances ?? 8,
            instanceCount: 0,
            herdr: host.herdr,
            providerBinding: host.providerBinding ?? "auto",
          };
      const count = instances.filter((i) => i.hostId === host.id).length;
      out.push({
        ...base,
        ssh: host.ssh,
        lastError: host.lastError,
        ...this.patches.get(host.id),
        id: host.id,
        instanceCount: count || host.instanceCount || base.instanceCount,
      });
    }
    if (useFixtures) {
      for (const extra of HOST_FIXTURES) {
        if (usedLabels.has(extra.label)) continue;
        usedLabels.add(extra.label);
        out.push({ ...extra, ...this.patches.get(extra.id) });
      }
    }
    for (const host of this.enrolled) {
      if (out.some((h) => h.id === host.id || h.label === host.label)) continue;
      out.push({ ...host, ...this.patches.get(host.id) });
    }
    return sortHostsOnlineFirst(out);
  }
}

export const hostRegistry = new HostRegistry();

export function useHostViews(hubHosts: Host[], instances: Instance[]): HostView[] {
  useSyncExternalStore(hostRegistry.subscribe, hostRegistry.snapshot, hostRegistry.snapshot);
  return hostRegistry.views(hubHosts, instances);
}
