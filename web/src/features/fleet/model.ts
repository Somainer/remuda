import type { Instance } from "../../types/instance";
import type { Id } from "../../types/wire";
import { projectStatus } from "../../lib/status";
import type { HostView, Placement } from "../hosts/model";

export type FleetMember = {
  instanceId: Id;
  hostId: Id;
  hostLabel: string;
  title: string;
  status: ReturnType<typeof projectStatus> | "cancelled";
  hostOnline: boolean;
};

export type Fleet = {
  id: Id;
  label: string;
  placement: Placement;
  members: FleetMember[];
};

export type FleetCounts = {
  working: number;
  idle: number;
  blocked: number;
  unknown: number;
  cancelled: number;
  offlineHosts: number;
};

export function countFleet(members: FleetMember[]): FleetCounts {
  const counts: FleetCounts = { working: 0, idle: 0, blocked: 0, unknown: 0, cancelled: 0, offlineHosts: 0 };
  const offline = new Set<string>();
  for (const member of members) {
    if (!member.hostOnline) offline.add(member.hostId);
    if (member.status === "cancelled") counts.cancelled += 1;
    else if (member.status === "working" || member.status === "starting") counts.working += 1;
    else if (member.status === "idle") counts.idle += 1;
    else if (member.status === "blocked") counts.blocked += 1;
    else counts.unknown += 1;
  }
  counts.offlineHosts = offline.size;
  return counts;
}

export function memberFromInstance(instance: Instance, host: HostView | undefined, title: string): FleetMember {
  return {
    instanceId: instance.id,
    hostId: instance.hostId,
    hostLabel: host?.label ?? instance.hostId.slice(0, 8),
    title,
    status: projectStatus(instance),
    hostOnline: host?.online ?? false,
  };
}
