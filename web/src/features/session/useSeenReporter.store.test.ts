import { act, renderHook } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { mockDb } from "../../lib/mock";
import { hubStore, useHub } from "../../lib/store";
import type { Instance } from "../../types/instance";
import type { Id, U64 } from "../../types/wire";
import { SEEN_RENEW_MS, useSeenReporter } from "./useSeenReporter";

afterEach(() => {
  vi.useRealTimers();
  hubStore.logout();
  vi.restoreAllMocks();
});

const ID = "ins_seen_loop" as Id;

async function boot(first: Instance) {
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [first], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(api, "settlementSubscribe").mockReturnValue(() => undefined);
  await hubStore.bootstrap();
}

/** Let rejected reports, rollbacks, re-renders and effects run to quiescence. */
async function settle() {
  for (let round = 0; round < 40; round += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

it("a failing Hub gets one report per renewal from the session page, never a report loop", async () => {
  await boot({
    ...mockDb.instances[0], id: ID, durableSeq: "6" as U64, unseen: true, turnEndSeq: "6" as U64, seenSeq: undefined,
  });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  vi.useFakeTimers();
  // Every report fails: the optimistic seen is applied and rolled back each
  // time. Past a cap the mock stops answering, so a regression to a report
  // loop fails this test on the count instead of exhausting the worker.
  const post = vi.spyOn(api, "instanceSeen").mockImplementation(() =>
    post.mock.calls.length > 50 ? new Promise(() => undefined) : Promise.reject(new Error("503")));
  // Wired exactly like SessionPage: the row comes from the live store.
  const hook = renderHook(() => {
    const row = useHub().instances.find((instance) => instance.id === ID);
    useSeenReporter(ID, row?.durableSeq);
  });
  await settle();
  expect(post).toHaveBeenCalledTimes(1);
  for (let tick = 1; tick <= 3; tick += 1) {
    await act(async () => {
      vi.advanceTimersByTime(SEEN_RENEW_MS);
    });
    await settle();
    expect(post).toHaveBeenCalledTimes(1 + tick);
  }
  // Each failure was rolled back: the row still reads unseen, as on the Hub.
  expect(hubStore.getSnapshot().instances.find((instance) => instance.id === ID)?.unseen).toBe(true);
  hook.unmount();
});
