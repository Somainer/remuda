import { describe, expect, it } from "vitest";
import type { Task, TaskState } from "../../types/generated";
import {
  BOARD_COLUMNS,
  boardColumn,
  canTransition,
  columnMoveHops,
  displayKeysByTaskId,
  formatDisplayKey,
  isFailedCard,
  lockedDepIds,
  reachableColumns,
} from "./boardColumns";

/** Minimal task fixture; only the fields the projection reads. */
function task(state: TaskState, patch: Partial<Task> = {}): Task {
  return {
    id: `tsk_${state}_${Math.random().toString(36).slice(2, 8)}`,
    revision: "1",
    createdAt: "2026-09-20T10:00:00.000Z",
    updatedAt: "2026-09-20T10:00:00.000Z",
    projectId: "prj_board",
    title: state,
    mandate: { chain: [] },
    class: "implement",
    state,
    ...patch,
  } as Task;
}

describe("boardColumn state projection", () => {
  it("maps the four queued states to todo", () => {
    for (const state of ["pending", "placed", "deferred", "parked"] as const) {
      expect(boardColumn(task(state))).toBe("todo");
    }
  });

  it("maps the working states to in-progress", () => {
    expect(boardColumn(task("running"))).toBe("in-progress");
    expect(boardColumn(task("stalled"))).toBe("in-progress");
  });

  it("maps only done to done", () => {
    expect(boardColumn(task("done"))).toBe("done");
  });

  it("derives the failed column from placement and flags the badge", () => {
    const early = task("failed");
    expect(boardColumn(early)).toBe("todo");
    expect(isFailedCard(early)).toBe(true);

    const midFlight = task("failed", {
      placement: { instanceId: "ins_1" },
      blockedReason: "worker exited 42",
    });
    expect(boardColumn(midFlight)).toBe("in-progress");
    // Not a fifth column, never folded into done.
    expect(boardColumn(midFlight)).not.toBe("done");
    expect(boardColumn(midFlight)).not.toBe("archived");

    // Placement clearing moves the failed card back to todo without a state
    // edit or any pre-fail storage.
    const unplaced = task("failed", { placement: null });
    expect(boardColumn(unplaced)).toBe("todo");
  });

  it("projects every state into one of the four columns", () => {
    const states: TaskState[] = [
      "pending",
      "placed",
      "running",
      "stalled",
      "done",
      "failed",
      "parked",
      "deferred",
    ];
    for (const state of states) {
      expect(BOARD_COLUMNS).toContain(boardColumn(task(state)));
    }
  });
});

describe("archive is orthogonal to state", () => {
  const archivedAt = "2026-09-20T12:00:00.000Z";

  it("wins over every state without the card leaving the state machine", () => {
    for (const state of [
      "pending",
      "placed",
      "running",
      "stalled",
      "done",
      "failed",
    ] as const) {
      const card = task(state, { archivedAt });
      expect(boardColumn(card)).toBe("archived");
      expect(card.state).toBe(state);
    }
  });

  it("still derives failed-by-placement inside the archive group", () => {
    // Archived wins first; state/placement stay untouched for un-archive.
    const card = task("failed", {
      archivedAt,
      placement: { instanceId: "ins_1" },
    });
    expect(boardColumn(card)).toBe("archived");
    expect(card.placement).toBeDefined();
  });

  it("has no drag moves and cannot be archived via a column move", () => {
    const card = task("running", { archivedAt });
    expect(columnMoveHops(card, "todo")).toBeNull();
    expect(columnMoveHops(card, "in-progress")).toBeNull();
    expect(columnMoveHops(card, "archived")).toBeNull();
    expect(reachableColumns(card)).toEqual([]);
  });
});

describe("column drag multi-hop mapping", () => {
  it("multi-hops pending and deferred through placed to running", () => {
    expect(columnMoveHops(task("pending"), "in-progress")).toEqual([
      "placed",
      "running",
    ]);
    expect(columnMoveHops(task("deferred"), "in-progress")).toEqual([
      "placed",
      "running",
    ]);
  });

  it("single-hops placed and parked to running", () => {
    expect(columnMoveHops(task("placed"), "in-progress")).toEqual(["running"]);
    expect(columnMoveHops(task("parked"), "in-progress")).toEqual(["running"]);
  });

  it("moves running to done and recovers stalled through running", () => {
    expect(columnMoveHops(task("running"), "done")).toEqual(["done"]);
    expect(columnMoveHops(task("stalled"), "done")).toEqual([
      "running",
      "done",
    ]);
  });

  it("back-drags in-progress cards to parked in the todo column", () => {
    expect(columnMoveHops(task("running"), "todo")).toEqual(["parked"]);
    expect(columnMoveHops(task("stalled"), "todo")).toEqual(["parked"]);
  });

  it("every planned hop is a legal state-machine edge", () => {
    const cases: Array<[TaskState, "todo" | "in-progress" | "done"]> = [
      ["pending", "in-progress"],
      ["deferred", "in-progress"],
      ["placed", "in-progress"],
      ["parked", "in-progress"],
      ["running", "done"],
      ["stalled", "done"],
      ["running", "todo"],
      ["stalled", "todo"],
    ];
    for (const [state, target] of cases) {
      const hops = columnMoveHops(task(state), target);
      expect(hops, `${state} → ${target}`).not.toBeNull();
      let current: TaskState = state;
      for (const hop of hops!) {
        expect(canTransition(current, hop), `${current} → ${hop}`).toBe(true);
        current = hop;
      }
    }
  });

  it("rejects cross-column shortcuts and same-column drags as a whole", () => {
    // A todo card cannot skip straight to done; no partial hops are sent.
    expect(columnMoveHops(task("pending"), "done")).toBeNull();
    // Done/archived are not drag targets from the work columns.
    expect(columnMoveHops(task("running"), "archived")).toBeNull();
    // Same column is not a move.
    expect(columnMoveHops(task("pending"), "todo")).toBeNull();
    expect(columnMoveHops(task("running"), "in-progress")).toBeNull();
  });

  it("never plans moves out of terminal states", () => {
    const done = task("done");
    const failed = task("failed", { placement: { instanceId: "ins_1" } });
    for (const card of [done, failed]) {
      expect(reachableColumns(card)).toEqual([]);
      for (const column of ["todo", "in-progress", "done"] as const) {
        expect(columnMoveHops(card, column)).toBeNull();
      }
    }
  });

  it("lists exactly the columns the current state can reach", () => {
    expect(reachableColumns(task("pending"))).toEqual(["in-progress"]);
    expect(reachableColumns(task("placed"))).toEqual(["in-progress"]);
    expect(reachableColumns(task("running"))).toEqual(["todo", "done"]);
    expect(reachableColumns(task("stalled"))).toEqual(["todo", "done"]);
  });
});

describe("the done column is not the dependency gate", () => {
  it("keeps edges locked for done-but-unlanded upstreams", () => {
    const dep = task("done");
    const dependent = task("pending", {
      projectId: dep.projectId,
      deps: [{ taskId: dep.id }],
    });
    const index = new Map([[dep.id, dep]]);
    expect(lockedDepIds(dependent, index)).toEqual([dep.id]);
  });

  it("unlocks only once the upstream carries a landed sha", () => {
    const dep = task("done", { landedSha: "abcdef7" });
    const dependent = task("pending", {
      projectId: dep.projectId,
      deps: [{ taskId: dep.id }],
    });
    expect(lockedDepIds(dependent, new Map([[dep.id, dep]]))).toEqual([]);
  });

  it("never unlocks an edge to a missing upstream", () => {
    const dependent = task("pending", { deps: [{ taskId: "tsk_gone" }] });
    expect(lockedDepIds(dependent, new Map())).toEqual(["tsk_gone"]);
  });
});

describe("display-only SE-nn keys", () => {
  it("formats two-digit sequence numbers", () => {
    expect(formatDisplayKey(1)).toBe("SE-01");
    expect(formatDisplayKey(42)).toBe("SE-42");
  });

  it("numbers per project in oldest-first order without storage", () => {
    const a1 = task("pending", {
      id: "tsk_a1",
      projectId: "prj_a",
      createdAt: "2026-09-20T09:00:00.000Z",
    });
    const b1 = task("pending", {
      id: "tsk_b1",
      projectId: "prj_b",
      createdAt: "2026-09-20T08:00:00.000Z",
    });
    const a2 = task("running", {
      id: "tsk_a2",
      projectId: "prj_a",
      createdAt: "2026-09-20T10:00:00.000Z",
    });
    // Deliberately out of order at the input.
    const keys = displayKeysByTaskId([a2, b1, a1]);
    expect(keys.get("tsk_a1")).toBe("SE-01");
    expect(keys.get("tsk_a2")).toBe("SE-02");
    expect(keys.get("tsk_b1")).toBe("SE-01");
    // Derived only: the source task documents carry no key field.
    expect((a1 as Record<string, unknown>).displayKey).toBeUndefined();
  });
});
