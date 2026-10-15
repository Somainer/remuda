import { describe, expect, it } from "vitest";
import type { OutboxRecord } from "./outbox";
import { leaseWakeDecision, reconcileLeaseWakes } from "./leaseWakes";

/**
 * c-reconnfu gate 20 item 1: the central wake policy as a decision TABLE —
 * every durable row state × ownership/activity/expiry combination maps to
 * exactly one obligation: deliver now, one deadline wake, held-only, quiet.
 */

const NOW = 100_000;

function rec(over: Partial<OutboxRecord>): OutboxRecord {
  return {
    commandId: over.commandId ?? "cmd_table",
    clientRequestId: "local_table",
    instanceId: "ins_table",
    prompt: "table",
    createdAt: NOW - 1_000,
    attempts: 0,
    state: "pending",
    ...over,
  };
}

const none = () => false;
const active = (id: string) => id === "cmd_active";

type Expect = "deliver" | "deadline" | "held" | "quiet";

const TABLE: Array<{ name: string; row: Partial<OutboxRecord>; active?: boolean; want: Expect; until?: number }> = [
  // Pending: deliver unless this document is already POSTing it.
  { name: "pending", row: { state: "pending" }, want: "deliver" },
  { name: "pending actively POSTed by this document", row: { state: "pending", commandId: "cmd_active" }, active: true, want: "quiet" },
  // Held: only the held-retry clock, regardless of lease fields.
  { name: "held", row: { state: "held" }, want: "held" },
  { name: "held actively POSTed by this document", row: { state: "held", commandId: "cmd_active" }, active: true, want: "held" },
  // Inflight, no lease (lease lost / restored): deliverable.
  { name: "inflight lease-less", row: { state: "inflight", lease: undefined }, want: "deliver" },
  // Inflight with a FRESH lease: one deadline wake — owner-agnostic.
  { name: "inflight fresh FOREIGN lease", row: { state: "inflight", lease: { owner: "owner_foreign", until: NOW + 30_000 } }, want: "deadline", until: NOW + 30_000 },
  { name: "inflight fresh SAME-owner lease (cloned sibling)", row: { state: "inflight", lease: { owner: "owner_self", until: NOW + 30_000 } }, want: "deadline", until: NOW + 30_000 },
  // A fresh lease THIS document is POSTing is the in-flight delivery: quiet.
  { name: "inflight fresh lease, this document active", row: { state: "inflight", commandId: "cmd_active", lease: { owner: "owner_self", until: NOW + 30_000 } }, active: true, want: "quiet" },
  // Inflight with an EXPIRED lease (any owner): stealable now.
  { name: "inflight EXPIRED foreign lease", row: { state: "inflight", lease: { owner: "owner_foreign", until: NOW - 1 } }, want: "deliver" },
  { name: "inflight EXPIRED same-owner lease", row: { state: "inflight", lease: { owner: "owner_self", until: NOW } }, want: "deliver" },
  // Terminal states: nothing.
  { name: "sent", row: { state: "sent", gotResponse: true }, want: "quiet" },
  { name: "done", row: { state: "done" }, want: "quiet" },
  { name: "rejected", row: { state: "rejected" }, want: "quiet" },
  { name: "unknown", row: { state: "unknown" }, want: "quiet" },
  { name: "reconciling", row: { state: "reconciling" }, want: "quiet" },
];

describe("leaseWakeDecision (gate 20 central wake policy)", () => {
  for (const c of TABLE) {
    it(c.name, () => {
      const d = leaseWakeDecision(rec(c.row), { now: NOW, isActiveDelivery: c.active ? active : none });
      expect(d.kind).toBe(c.want);
      expect(d.commandId).toBe(c.row.commandId ?? "cmd_table");
      expect(d.instanceId).toBe("ins_table");
      if (c.want === "deadline") {
        expect(d.lease?.until).toBe(c.until);
        expect(d.lease?.owner).toBe(c.row.lease?.owner);
      } else {
        expect(d.lease).toBeUndefined();
      }
    });
  }
});

describe("reconcileLeaseWakes", () => {
  it("maps every row of an authoritative snapshot to one decision, preserving order", () => {
    const rows = [
      rec({ commandId: "cmd_pending", state: "pending" }),
      rec({ commandId: "cmd_foreign", state: "inflight", lease: { owner: "owner_b", until: NOW + 5_000 } }),
      rec({ commandId: "cmd_held", state: "held" }),
      rec({ commandId: "cmd_done", state: "done" }),
      rec({ commandId: "cmd_expired", state: "inflight", lease: { owner: "owner_b", until: NOW - 10 } }),
    ];
    const decisions = reconcileLeaseWakes(rows, { now: NOW, isActiveDelivery: none });
    expect(decisions.map((d) => [d.commandId, d.kind])).toEqual([
      ["cmd_pending", "deliver"],
      ["cmd_foreign", "deadline"],
      ["cmd_held", "held"],
      ["cmd_done", "quiet"],
      ["cmd_expired", "deliver"],
    ]);
    expect(decisions[1]?.lease).toEqual({ owner: "owner_b", until: NOW + 5_000 });
  });

  it("treats a clock-rolled-back world purely from wall time: a not-yet-due lease is a deadline, a fire can never steal it", () => {
    // A timer armed at until=40_000 (old wall clock) fires after the clock
    // rolled BACK to 30_000: the policy re-judges with the fresh wall time
    // and keeps waiting until 40_000 instead of stealing a fresh lease.
    const d = leaseWakeDecision(
      rec({ state: "inflight", lease: { owner: "owner_b", until: 40_000 } }),
      { now: 30_000, isActiveDelivery: none },
    );
    expect(d.kind).toBe("deadline");
    expect(d.lease?.until).toBe(40_000);
  });
});
