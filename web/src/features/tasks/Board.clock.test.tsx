import { act, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BoardPage } from "./Board";

/**
 * c-perffu r2 regression: with quiet polls no longer emitting (equal
 * payloads keep row/array identity), relative-time labels must still advance
 * on the board's own display clock — the clock never goes back into the data
 * emit. Fake timers: the hub snapshot never changes, the board projection
 * keeps the SAME object on every 5 s poll, yet 刚刚 → 1m crosses.
 */

const T0 = new Date("2026-09-29T10:00:00Z").getTime();
const sessionIso = new Date(T0 - 10_000).toISOString();

// One frozen reference: every /v1/board poll returns this exact object,
// simulating an unchanged projection (useBoardView keeps identity).
const boardView = {
  columns: {
    todo: [
      {
        id: "tsk_1",
        boardColumn: "todo",
        displayKey: "SE-01",
        title: "task one",
        state: "pending",
        archivedAt: null,
        blockedReason: null,
        landedSha: null,
        placement: null,
      },
    ],
    "in-progress": [],
    done: [],
    archived: [],
  },
};

// Also one frozen reference: useHub's snapshot never changes identity, and
// the test never calls a store listener — there is no data emission at all.
const hub = {
  workspaces: [],
  instances: [
    {
      id: "ins_1",
      taskId: "tsk_1",
      kind: "claude",
      name: "s1",
      lifecycle: "ready",
      updatedAt: sessionIso,
    },
  ],
  interactions: [],
};

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
}));

vi.mock("../../lib/api", () => ({
  rest: vi.fn(async (path: string) =>
    path.startsWith("/v1/board")
      ? boardView
      : path.startsWith("/v1/projects")
        ? { items: [] }
        : { items: [] },
  ),
  HubHttpError: class HubHttpError extends Error {
    status = 400;
  },
}));

vi.mock("./TaskList", () => ({
  // Rail is covered by its own component tests; isolate the card surface.
  TaskGroups: () => null,
  useLiveBranches: () => ({ branchOf: () => null }),
}));

vi.mock("../spaces/SpacesPanel", () => ({
  HarnessGlyph: () => null,
}));

function renderBoard() {
  return render(
    <MemoryRouter initialEntries={["/board"]}>
      <BoardPage />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("BoardPage relative-time clock", () => {
  it("advances 刚刚 → 1m while the projection object and hub snapshot stay identical", async () => {
    const { unmount } = renderBoard();
    // Flush the first (frozen-reference) /v1/board poll microtasks.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    // Initial label: 10 s old.
    expect(screen.getByText("刚刚")).toBeTruthy();

    // Cross the minute boundary on the local 30 s display clock. The 5 s
    // board polls also fire, but resolve the SAME projection reference.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(61_000);
    });

    expect(screen.getByText("1m")).toBeTruthy();
    expect(screen.queryByText("刚刚")).toBeNull();
    unmount();
  });
});
