/**
 * home.md §8 新鲜度 — the store half: the time of the last successful list
 * read and the start of the current run of consecutive failures, stamped from
 * the injectable clock so tests never depend on wall-clock milliseconds.
 */
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { api } from "./api";
import { mockPage } from "./mock";
import { setClock } from "./clock";
import { hubStore } from "./store";

let clock = 1_000;
let restoreClock: () => void;

/** A successful list + interaction read pair. */
function mockRead() {
  vi.spyOn(api, "instanceList").mockResolvedValue(mockPage([]));
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
}

beforeEach(() => {
  clock = 1_000;
  restoreClock = setClock(() => clock);
});

afterEach(() => {
  restoreClock();
  vi.restoreAllMocks();
});

it("stamps the last successful refresh from the injected clock", async () => {
  // A store that has never read anything is unknown, not fresh.
  expect(hubStore.getRefreshHealth().lastOkAt).toBeNull();
  mockRead();
  clock = 5_000;
  await hubStore.refresh();
  expect(hubStore.getRefreshHealth()).toMatchObject({ lastOkAt: 5_000, failingSince: null });
  clock = 7_000;
  await hubStore.refresh();
  expect(hubStore.getRefreshHealth()).toMatchObject({ lastOkAt: 7_000, failingSince: null });
});

it("keeps the FIRST failure's start across a run and clears it on the next success", async () => {
  mockRead();
  clock = 10_000;
  await hubStore.refresh();
  expect(hubStore.getRefreshHealth().failingSince).toBeNull();
  const instanceList = vi.spyOn(api, "instanceList");
  instanceList.mockRejectedValue(new Error("hub unreachable"));
  clock = 12_000;
  await expect(hubStore.refresh()).rejects.toThrow("hub unreachable");
  expect(hubStore.getRefreshHealth().failingSince).toBe(12_000);
  // A later failure extends the run; it does not restart the banner debounce.
  clock = 14_000;
  await expect(hubStore.refresh()).rejects.toThrow("hub unreachable");
  expect(hubStore.getRefreshHealth().failingSince).toBe(12_000);
  // A success ends the run and stamps the new read.
  instanceList.mockResolvedValue(mockPage([]));
  clock = 16_000;
  await hubStore.refresh();
  expect(hubStore.getRefreshHealth()).toMatchObject({ lastOkAt: 16_000, failingSince: null });
});

it("counts every settled refresh, success or failure, so a grace can end on either", async () => {
  mockRead();
  const before = hubStore.getRefreshHealth().settledSeq;
  await hubStore.refresh();
  expect(hubStore.getRefreshHealth().settledSeq).toBe(before + 1);
  vi.spyOn(api, "instanceList").mockRejectedValue(new Error("down"));
  await expect(hubStore.refresh()).rejects.toThrow("down");
  expect(hubStore.getRefreshHealth().settledSeq).toBe(before + 2);
});

it("notifies freshness subscribers without touching the list snapshot identity", async () => {
  mockRead();
  await hubStore.refresh();
  const snapshot = hubStore.getSnapshot();
  const listener = vi.fn();
  const off = hubStore.subscribeRefreshHealth(listener);
  await hubStore.refresh();
  expect(listener).toHaveBeenCalled();
  // A quiet successful poll still must not replace the Hub snapshot: the
  // commit:HomeList contract depends on it.
  expect(hubStore.getSnapshot()).toBe(snapshot);
  off();
});
