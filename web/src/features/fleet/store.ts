import { useSyncExternalStore } from "react";
import type { Instance } from "../../types/instance";
import type { Id } from "../../types/wire";
import { hostsMatching, type HostView, type Placement } from "../hosts/model";
import { memberFromInstance, type Fleet, type FleetMember } from "./model";

type Listener = () => void;

const DEMO_ID = "obj_01993ab0-0000-7000-8000-00000000f001" as Id;

class FleetStore {
  private fleets: Fleet[] = [];
  private cancelled = new Set<string>();
  private listeners = new Set<Listener>();
  private seeded = false;
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

  seed(instances: Instance[], hosts: HostView[], titleOf: (id: string) => string) {
    if (this.seeded) return;
    this.seeded = true;
    const members: FleetMember[] = instances.slice(0, 3).map((instance) => {
      const host = hosts.find((h) => h.id === instance.hostId) ?? hosts[0];
      return memberFromInstance(instance, host, titleOf(instance.id));
    });
    for (const host of hosts.filter((h) => h.transport !== "outbound-wss").slice(0, 2)) {
      members.push({
        instanceId: `ins_01993ab0-0000-7000-8000-${host.id.slice(-12)}` as Id,
        hostId: host.id,
        hostLabel: host.label,
        title: `fleet · ${host.label}`,
        status: host.online ? "idle" : "unknown",
        hostOnline: host.online,
      });
    }
    this.fleets = [
      {
        id: DEMO_ID,
        label: "print canary",
        placement: { kind: "any" },
        members,
      },
    ];
    this.emit();
  }

  list(): Fleet[] {
    return this.fleets.map((fleet) => ({
      ...fleet,
      members: fleet.members.map((member) =>
        this.cancelled.has(member.instanceId) ? { ...member, status: "cancelled" } : member,
      ),
    }));
  }

  /**
   * TODO(remuda-hub): POST /v1/fleet/instances
   * Same InstanceSpec on N hosts; returns instanceIds. No silent downgrade.
   */
  create(placement: Placement, hosts: HostView[], n: number, label: string): Fleet {
    const matched = hostsMatching(hosts, placement);
    if (!matched.length) throw new Error("PLACEMENT_UNSATISFIED");
    const chosen = matched.slice(0, Math.max(1, n));
    const fleet: Fleet = {
      id: `obj_01993ab0-0000-7000-8000-${Date.now().toString(16).slice(-12).padStart(12, "0")}` as Id,
      label,
      placement,
      members: chosen.map((host) => ({
        instanceId: `ins_01993ab0-0000-7000-8000-${host.id.slice(-12)}` as Id,
        hostId: host.id,
        hostLabel: host.label,
        title: `${label} · ${host.label}`,
        status: host.online ? "starting" : "unknown",
        hostOnline: host.online,
      })),
    };
    this.fleets = [fleet, ...this.fleets];
    this.emit();
    return fleet;
  }

  /** TODO(remuda-hub): Command broadcast cancel across fleet instanceIds. */
  cancel(fleetId: Id) {
    const fleet = this.fleets.find((item) => item.id === fleetId);
    if (!fleet) return;
    for (const member of fleet.members) this.cancelled.add(member.instanceId);
    this.emit();
  }
}

export const fleetStore = new FleetStore();

export function useFleets(): Fleet[] {
  useSyncExternalStore(fleetStore.subscribe, fleetStore.snapshot, fleetStore.snapshot);
  return fleetStore.list();
}
