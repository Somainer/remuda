import { render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { hubStore } from "../../lib/store";
import type { OwnerGlance } from "../../types/generated";
import { __glancePollRefs, __stopGlancePollForTest, useOwnerGlancePoll } from "./useOwnerGlance";

/**
 * home-glance-d060 §5.3 / item 9: the home poll issues EXACTLY ONE
 * `GET /v1/owner/glance` per 2 s cycle — not three — and only while a home
 * surface is mounted. The spy is on the real API layer, so the retired
 * per-row journal read (`api.eventsRead`) is asserted absent too.
 */

const GLANCE: OwnerGlance = {
  generatedAt: "2026-10-30T10:09:00.000Z",
  timezoneOffsetMinutes: 0,
  counts: { needYou: 1, doneUnseen: 0, working: 2, doneToday: 3 },
  doneUnseenIds: [],
  waiting: [],
  working: [],
  more: { waiting: 0, working: 0 },
};

function Probe() {
  useOwnerGlancePoll();
  return null;
}

afterEach(() => {
  __stopGlancePollForTest();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

it("fetches once at mount, then exactly once per cycle, and stops at unmount", async () => {
  vi.useFakeTimers();
  const ownerGlance = vi.spyOn(api, "ownerGlance").mockResolvedValue(GLANCE);
  const eventsRead = vi.spyOn(api, "eventsRead").mockRejectedValue(new Error("no journal reads"));

  const { unmount } = render(<Probe />);
  await vi.advanceTimersByTimeAsync(0);
  // Item 10: the mount read — never 「–」 for a whole poll period.
  expect(ownerGlance).toHaveBeenCalledTimes(1);
  expect(__glancePollRefs()).toBe(1);

  await vi.advanceTimersByTimeAsync(2_000);
  expect(ownerGlance).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(4_000);
  // Two more cycles, one request each: no triple-fire per poll.
  expect(ownerGlance).toHaveBeenCalledTimes(4);
  expect(hubStore.getSnapshot().glance).toEqual(GLANCE);

  unmount();
  expect(__glancePollRefs()).toBe(0);
  await vi.advanceTimersByTimeAsync(6_000);
  expect(ownerGlance).toHaveBeenCalledTimes(4);
  // The whole point: the home poll never falls back to per-row journal reads.
  expect(eventsRead).not.toHaveBeenCalled();
  // Positive control: the spy is wired to the real API path and can fire.
  await api.eventsRead({ journalId: "jrn_control", afterSeq: "0", limit: 1 } as never).catch(
    () => undefined,
  );
  expect(eventsRead).toHaveBeenCalledTimes(1);
});

it("keeps one interval for two mounted readers (a shared poll)", async () => {
  vi.useFakeTimers();
  const ownerGlance = vi.spyOn(api, "ownerGlance").mockResolvedValue(GLANCE);
  const first = render(<Probe />);
  const second = render(<Probe />);
  await vi.advanceTimersByTimeAsync(0);
  // The second mount joins the live interval instead of starting its own.
  expect(ownerGlance).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(2_000);
  expect(ownerGlance).toHaveBeenCalledTimes(2);
  first.unmount();
  await vi.advanceTimersByTimeAsync(2_000);
  expect(ownerGlance).toHaveBeenCalledTimes(3);
  second.unmount();
  await vi.advanceTimersByTimeAsync(4_000);
  expect(ownerGlance).toHaveBeenCalledTimes(3);
});
