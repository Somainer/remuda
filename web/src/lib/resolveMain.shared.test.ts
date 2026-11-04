import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { mockDb } from "./mock";
import { resolveMain } from "./store";
import type { Instance } from "../types/instance";
import type { Id } from "../types/wire";

/**
 * home.md §5.3 / §6.1: ONE Main resolution. The Hub's owner glance
 * (`owner_glance::pick_main_seat`) and this client resolver run the SAME cases
 * from the same file, so a paused or closed Main names the same chapter — and
 * therefore the same subtree — on both sides.
 */

type Seat = {
  instanceId: string;
  lineageId: string;
  generation: number;
  lifecycle: Instance["lifecycle"];
  lastError: string | null;
  fenced: boolean;
  timeMs: number;
};

const cases = JSON.parse(
  readFileSync(
    `${import.meta.dirname}/../../../crates/remuda-hub/tests/fixtures/main_seat_cases.json`,
    "utf8",
  ),
) as { cases: { name: string; seats: Seat[]; expect: string | null }[] };

function seatRow(seat: Seat): Instance {
  const at = new Date(seat.timeMs).toISOString();
  return {
    ...mockDb.instances[0],
    id: seat.instanceId as Id,
    lineageId: seat.lineageId as Id,
    generation: seat.generation,
    lifecycle: seat.lifecycle,
    lastError: seat.lastError,
    fencedAt: seat.fenced ? at : undefined,
    grants: ["address-owner"],
    createdAt: at,
    updatedAt: at,
  } as Instance;
}

describe("resolveMain follows the shared Hub/client cases", () => {
  it("has the contract's cases", () => {
    expect(cases.cases.length).toBeGreaterThanOrEqual(10);
  });

  for (const testCase of cases.cases) {
    it(testCase.name, () => {
      const rows = testCase.seats.map(seatRow);
      expect(resolveMain(rows)?.instance.id ?? null).toBe(testCase.expect);
      // Order-independent, like the Hub's: never "whichever was read first".
      expect(resolveMain([...rows].reverse())?.instance.id ?? null).toBe(testCase.expect);
    });
  }
});
