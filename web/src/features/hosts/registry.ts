import { useSyncExternalStore } from "react";
import type { Host, Instance } from "../../types/instance";
import { HOST_FIXTURES } from "./fixtures";
import { carrierOf, hostOnline, type HostView } from "./model";

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
      const extra = HOST_FIXTURES.find((f) => f.label === host.label);
      usedLabels.add(host.label);
      const base: HostView = extra
        ? { ...extra, id: host.id, state: host.state, online: hostOnline(host.state) }
        : {
            id: host.id,
            label: host.label,
            state: host.state,
            online: hostOnline(host.state),
            transport: carrierOf(host.transport.mode),
            hostname: host.hostname,
            port: host.port,
            cli: [],
            labels: [],
            maxInstances: 4,
            instanceCount: 0,
          };
      const count = instances.filter((i) => i.hostId === host.id).length;
      out.push({
        ...base,
        ...this.patches.get(host.id),
        id: host.id,
        instanceCount: count || base.instanceCount,
      });
    }
    for (const extra of HOST_FIXTURES) {
      if (usedLabels.has(extra.label)) continue;
      usedLabels.add(extra.label);
      out.push({ ...extra, ...this.patches.get(extra.id) });
    }
    for (const host of this.enrolled) {
      if (out.some((h) => h.id === host.id || h.label === host.label)) continue;
      out.push({ ...host, ...this.patches.get(host.id) });
    }
    return out;
  }
}

export const hostRegistry = new HostRegistry();

export function useHostViews(hubHosts: Host[], instances: Instance[]): HostView[] {
  useSyncExternalStore(hostRegistry.subscribe, hostRegistry.snapshot, hostRegistry.snapshot);
  return hostRegistry.views(hubHosts, instances);
}
