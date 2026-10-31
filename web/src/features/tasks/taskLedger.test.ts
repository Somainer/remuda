import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Task } from "../../types/generated";

/**
 * home.md §5.1: the ① count reads /v1/tasks for B, and the phone task layer
 * renders the same ledger. One scope must mean ONE poller, however many
 * components subscribe — otherwise /m doubles the 5 s task poll.
 */
const restMock = vi.hoisted(() => vi.fn());

vi.mock("../../lib/api", () => ({ rest: restMock }));

import { resetTaskLedgerForTest, useTaskLedger } from "./taskLedger";

function task(id: string): Task {
  return { id, projectId: "prj_1", title: id } as unknown as Task;
}

const calls = (prefix: string) =>
  restMock.mock.calls.filter(([path]) => String(path).startsWith(prefix)).length;

beforeEach(() => {
  vi.useFakeTimers();
  // Module state outlives a test file: start every case with no cached scope.
  resetTaskLedgerForTest();
  restMock.mockReset();
  restMock.mockImplementation(async (path: string) =>
    String(path).startsWith("/v1/projects")
      ? { items: [{ id: "prj_1", name: "remuda" }] }
      : { items: [task("tsk_1")] },
  );
});

afterEach(() => {
  vi.useRealTimers();
});

describe("shared task ledger", () => {
  it("runs one poller for every subscriber on the same scope", async () => {
    const first = renderHook(() => useTaskLedger());
    const second = renderHook(() => useTaskLedger());
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(calls("/v1/tasks")).toBe(1);
    expect(calls("/v1/projects")).toBe(1);
    // Both hooks see the one published snapshot.
    expect(first.result.current.tasks.map((row) => row.id)).toEqual(["tsk_1"]);
    expect(second.result.current.tasks.map((row) => row.id)).toEqual(["tsk_1"]);
    expect(second.result.current.projectName("prj_1")).toBe("remuda");

    await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
    expect(calls("/v1/tasks")).toBe(2);

    first.unmount();
    await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
    expect(calls("/v1/tasks"), "the surviving subscriber keeps the poller").toBe(3);
    second.unmount();
    await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
    expect(calls("/v1/tasks"), "no subscriber stops it").toBe(3);
  });

  it("keeps a separate poller per project scope", async () => {
    const all = renderHook(() => useTaskLedger());
    const scoped = renderHook(() => useTaskLedger("prj_1"));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(calls("/v1/tasks")).toBe(2);
    expect(restMock.mock.calls.map(([path]) => path)).toEqual(
      expect.arrayContaining(["/v1/tasks", "/v1/tasks?project=prj_1"]),
    );
    all.unmount();
    scoped.unmount();
  });
});
