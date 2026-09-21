import { describe, expect, it } from "vitest";
import type { TaskState } from "../../types/generated";
import type { CardSession } from "./boardModel";
import {
  ARCHIVED_REASON,
  SAME_COLUMN_REASON,
  TERMINAL_REASON,
  bindingShareKey,
  buildBoardModel,
  configLabelOf,
  dropLegality,
  sharedCounts,
  type BoardItem,
  type BoardView,
} from "./boardModel";
import { boardColumn } from "./boardColumns";

/**
 * Fixtures mirror GET /v1/board: BoardItem is the task doc plus the
 * server-derived boardColumn and displayKey. The Hub is the projection
 * authority; the tests build the column the server would project.
 */
let seq = 0;
function item(
  state: TaskState,
  patch: Partial<BoardItem> = {},
): BoardItem {
  seq += 1;
  const base: BoardItem = {
    id: `tsk_${state}_${seq}`,
    revision: "1",
    createdAt: `2026-09-21T10:0${seq}:00.000Z`,
    updatedAt: `2026-09-21T10:0${seq}:00.000Z`,
    projectId: "prj_boardui",
    title: `card ${state} ${seq}`,
    mandate: { chain: [] },
    class: "implement",
    state,
    ...patch,
  } as BoardItem;
  return {
    ...base,
    boardColumn: boardColumn(base),
    displayKey: `SE-${String(seq).padStart(2, "0")}`,
  };
}

function viewOf(...items: BoardItem[]): BoardView {
  const view: BoardView = {
    project: "prj_boardui",
    columns: { todo: [], "in-progress": [], done: [], archived: [] },
  };
  for (const card of items) view.columns[card.boardColumn].push(card);
  return view;
}

function session(partial: Partial<CardSession> & Pick<CardSession, "id" | "taskId">): CardSession {
  return {
    kind: "claude",
    updatedAt: "2026-09-21T11:00:00.000Z",
    ...partial,
  };
}

/** Columns the card's precomputed drops map would mark legal. */
function allowedColumns(card: BoardItem): ("todo" | "in-progress" | "done")[] {
  return (["todo", "in-progress", "done"] as const).filter((column) =>
    dropLegality(card, column).allowed,
  );
}

describe("board column composition", () => {
  it("renders exactly three work columns populated from the server projection", () => {
    const pending = item("pending");
    const placed = item("placed");
    const running = item("running");
    const stalled = item("stalled");
    const done = item("done");

    const model = buildBoardModel({ view: viewOf(pending, placed, running, stalled, done) });

    expect(model.columns.map((column) => column.column)).toEqual([
      "todo",
      "in-progress",
      "done",
    ]);
    expect(model.columns[0].cards.map((card) => card.id).sort()).toEqual(
      [pending.id, placed.id].sort(),
    );
    expect(model.columns[1].cards.map((card) => card.id).sort()).toEqual(
      [running.id, stalled.id].sort(),
    );
    expect(model.columns[2].cards.map((card) => card.id)).toEqual([done.id]);
    expect(model.archived).toEqual([]);
  });

  it("places failed cards by placement: no placement → todo, held placement → in-progress", () => {
    const failedEarly = item("failed", { blockedReason: "supply exhausted" });
    const failedMid = item("failed", {
      placement: { instanceId: "ins_42" },
      blockedReason: "worker exited 42",
    });

    const model = buildBoardModel({ view: viewOf(failedEarly, failedMid) });

    const todoFailed = model.columns[0].cards.find((card) => card.id === failedEarly.id)!;
    const midFailed = model.columns[1].cards.find((card) => card.id === failedMid.id)!;

    expect(todoFailed.failed).toBe(true);
    expect(todoFailed.column).toBe("todo");
    expect(todoFailed.blockedReason).toBe("supply exhausted");
    // Never folded into done, never a fifth column.
    expect(model.columns[2].cards).toEqual([]);

    expect(midFailed.failed).toBe(true);
    expect(midFailed.column).toBe("in-progress");
    expect(midFailed.blockedReason).toBe("worker exited 42");
  });

  it("agrees with the pure helper projection for every ledger state", () => {
    const archivedAt = "2026-09-21T12:00:00.000Z";
    const cards = [
      item("pending"),
      item("placed"),
      item("deferred"),
      item("parked"),
      item("running"),
      item("stalled"),
      item("done"),
      item("failed"),
      item("failed", { placement: { instanceId: "ins_1" } }),
      item("running", { archivedAt }),
    ];
    const model = buildBoardModel({ view: viewOf(...cards) });
    for (const card of [...model.columns.flatMap((column) => column.cards), ...model.archived]) {
      expect(card.column).toBe(boardColumn(card.item));
    }
  });
});

describe("archive is a filter, never a fourth column", () => {
  const archivedAt = "2026-09-21T12:00:00.000Z";

  it("keeps archived cards in the archive fold regardless of their ledger state", () => {
    const active = item("running");
    const archivedRunning = item("stalled", { archivedAt });
    const archivedDone = item("done", { archivedAt });

    const model = buildBoardModel({ view: viewOf(active, archivedRunning, archivedDone) });

    expect(model.archived.map((card) => card.id).sort()).toEqual(
      [archivedRunning.id, archivedDone.id].sort(),
    );
    for (const column of model.columns) {
      expect(column.cards.some((card) => card.archived)).toBe(false);
    }
    // The state machine is untouched: the card keeps its state and badge rules.
    const stalledCard = model.archived.find((card) => card.id === archivedRunning.id)!;
    expect(stalledCard.item.state).toBe("stalled");
    expect(stalledCard.column).toBe("archived");
  });

  it("excludes nothing else: an empty archive fold leaves all work cards visible", () => {
    const model = buildBoardModel({ view: viewOf(item("pending"), item("done")) });
    expect(model.archived).toEqual([]);
    expect(model.columns[0].cards).toHaveLength(1);
    expect(model.columns[2].cards).toHaveLength(1);
  });
});

describe("precomputed drag legality and multi-hop sequences", () => {
  it("to-do → in-progress: pending/deferred multi-hop, placed/parked single hop", () => {
    expect(dropLegality(item("pending"), "in-progress").hops).toEqual(["placed", "running"]);
    expect(dropLegality(item("deferred"), "in-progress").hops).toEqual(["placed", "running"]);
    expect(dropLegality(item("placed"), "in-progress").hops).toEqual(["running"]);
    expect(dropLegality(item("parked"), "in-progress").hops).toEqual(["running"]);
  });

  it("in-progress → done: running single hop, stalled takes stalled→running→done", () => {
    expect(dropLegality(item("running"), "done").hops).toEqual(["done"]);
    expect(dropLegality(item("stalled"), "done").hops).toEqual(["running", "done"]);
  });

  it("in-progress → to-do lands on the nearest legal state, parked", () => {
    expect(dropLegality(item("running"), "todo").hops).toEqual(["parked"]);
    expect(dropLegality(item("stalled"), "todo").hops).toEqual(["parked"]);
  });

  it("precomputes the reachable column set per card", () => {
    expect(allowedColumns(item("pending"))).toEqual(["in-progress"]);
    expect(allowedColumns(item("running"))).toEqual(["todo", "done"]);
    expect(allowedColumns(item("stalled"))).toEqual(["todo", "done"]);
  });

  it("blocks to-do → done with an explicit reason (pass through in-progress)", () => {
    const legality = dropLegality(item("pending"), "done");
    expect(legality.allowed).toBe(false);
    expect(legality.hops).toEqual([]);
    expect(legality.reason).toBe("需先移到进行中列");
  });

  it("blocks every drop for terminal cards with a shape-bearing reason", () => {
    const failedEarly = item("failed", { blockedReason: "supply exhausted" });
    const failedMid = item("failed", { placement: { instanceId: "x" } });
    const done = item("done");

    for (const target of ["todo", "in-progress", "done"] as const) {
      const early = dropLegality(failedEarly, target);
      expect(early.allowed).toBe(false);
      expect(early.reason).toContain("supply exhausted");

      expect(dropLegality(failedMid, target).reason).toBe(TERMINAL_REASON);
      expect(dropLegality(done, target).reason).toBe(TERMINAL_REASON);
    }
    expect(allowedColumns(failedEarly)).toEqual([]);
    expect(allowedColumns(done)).toEqual([]);
  });

  it("blocks the card's own column and every move for an archived card", () => {
    expect(dropLegality(item("running"), "in-progress").reason).toBe(SAME_COLUMN_REASON);
    const archived = item("running", { archivedAt: "2026-09-21T12:00:00.000Z" });
    for (const target of ["todo", "in-progress", "done"] as const) {
      expect(dropLegality(archived, target).reason).toBe(ARCHIVED_REASON);
    }
  });

  it("attaches the drop table to every card in the model", () => {
    const pending = item("pending");
    const model = buildBoardModel({ view: viewOf(pending) });
    const card = model.byId.get(pending.id)!;
    expect(card.drops["in-progress"].allowed).toBe(true);
    expect(card.drops.done.allowed).toBe(false);
    expect(card.drops.done.reason).toBe("需先移到进行中列");
  });
});

describe("sharing count over the lease identity key", () => {
  const binding = (worktreeName?: string) => ({
    mode: "reuse" as const,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    ...(worktreeName ? { worktreeName } : {}),
  });

  it("counts tasks on the same (host, workspace, dir_key) and labels the footer", () => {
    const a = item("pending", { workspaceBinding: binding("agent-two") });
    const b = item("pending", { workspaceBinding: binding("agent-two") });
    const model = buildBoardModel({ view: viewOf(a, b) });

    for (const id of [a.id, b.id]) {
      const card = model.byId.get(id)!;
      expect(card.sharedCount).toBe(2);
      expect(card.sharedLabel).toBe("与 2 个 task 共用");
    }
  });

  it("counts reuse-to-root tasks against the '.' dir_key", () => {
    const a = item("pending", { workspaceBinding: binding() });
    const b = item("running", { workspaceBinding: binding() });
    const counts = sharedCounts([a, b]);
    expect(counts.get("hst_1|wsp_1|.")).toBe(2);
  });

  it("never merges different worktrees, workspaces or hosts (D-024)", () => {
    const a = item("pending", { workspaceBinding: binding("agent-two") });
    const otherDir = item("pending", { workspaceBinding: binding("agent-three") });
    const otherWsp = item("pending", {
      workspaceBinding: { ...binding("agent-two"), workspaceId: "wsp_2" },
    });
    const otherHost = item("pending", {
      workspaceBinding: { ...binding("agent-two"), hostId: "hst_2" },
    });
    const model = buildBoardModel({ view: viewOf(a, otherDir, otherWsp, otherHost) });
    expect(model.byId.get(a.id)!.sharedCount).toBe(1);
    expect(model.byId.get(a.id)!.sharedLabel).toBeNull();
  });

  it("does not share unbound tasks and counts archived holders too", () => {
    const unbound = item("pending");
    const active = item("running", { workspaceBinding: binding("agent-two") });
    const archived = item("done", {
      archivedAt: "2026-09-21T12:00:00.000Z",
      workspaceBinding: binding("agent-two"),
    });
    const model = buildBoardModel({ view: viewOf(unbound, active, archived) });
    expect(model.byId.get(unbound.id)!.sharedCount).toBe(1);
    expect(model.byId.get(active.id)!.sharedCount).toBe(2);
    expect(model.byId.get(archived.id)!.sharedLabel).toBe("与 2 个 task 共用");
  });

  it("keeps a reuse root distinct from a sibling worktree", () => {
    const root = item("pending", { workspaceBinding: binding() });
    const sibling = item("pending", { workspaceBinding: binding("agent-one") });
    const counts = sharedCounts([root, sibling]);
    expect(counts.get("hst_1|wsp_1|.")).toBe(1);
    expect(counts.get("hst_1|wsp_1|agent-one")).toBe(1);
    expect(bindingShareKey(root)).toBe("hst_1|wsp_1|.");
  });
});

describe("card sessions and the config label", () => {
  it("lists the task's sessions with placement first, then live, then exited", () => {
    const placed = item("running", { placement: { instanceId: "ins_placed" } });
    const instances = [
      session({ id: "ins_old", taskId: placed.id, lifecycle: "exited", updatedAt: "2026-09-21T09:00:00Z" }),
      session({ id: "ins_live", taskId: placed.id, lifecycle: "running", updatedAt: "2026-09-21T10:00:00Z" }),
      session({ id: "ins_placed", taskId: placed.id, lifecycle: "running", updatedAt: "2026-09-21T11:00:00Z" }),
      session({ id: "ins_other", taskId: "tsk_other", updatedAt: "2026-09-21T12:00:00Z" }),
    ];
    const model = buildBoardModel({ view: viewOf(placed), instances });
    const card = model.byId.get(placed.id)!;
    expect(card.sessionCount).toBe(3);
    expect(card.sessions.map((sessionRow) => sessionRow.id)).toEqual([
      "ins_placed",
      "ins_live",
      "ins_old",
    ]);
    expect(card.primarySessionId).toBe("ins_placed");
  });

  it("falls back to a live session when the placement id is gone", () => {
    const task = item("placed", { placement: { instanceId: "ins_gone" } });
    const instances = [
      session({ id: "ins_live", taskId: task.id, lifecycle: "ready" }),
    ];
    const model = buildBoardModel({ view: viewOf(task), instances });
    expect(model.byId.get(task.id)!.primarySessionId).toBe("ins_live");
  });

  it("has no session for an unplaced ledger task", () => {
    const task = item("pending");
    const model = buildBoardModel({ view: viewOf(task) });
    const card = model.byId.get(task.id)!;
    expect(card.sessionCount).toBe(0);
    expect(card.primarySessionId).toBeNull();
  });

  it("defaults the config label, then names the profile a session applies", () => {
    expect(configLabelOf([])).toBe("default");
    // The baseline native profile id `none` (D-012) reads as default.
    expect(
      configLabelOf([session({ id: "n", taskId: "t", providerProfileId: "none" })]),
    ).toBe("default");
    expect(
      configLabelOf([session({ id: "a", taskId: "t", providerProfileId: "strict" })]),
    ).toBe("strict");
  });
});

describe("board search", () => {
  it("filters cards across work columns and the archive fold by key/title/reason", () => {
    const alpha = item("pending", { title: "fix spill handler" });
    const beta = item("running", { title: "unrelated work" });
    const archived = item("done", {
      title: "spill follow-up",
      archivedAt: "2026-09-21T12:00:00.000Z",
    });
    const view = viewOf(alpha, beta, archived);

    const filtered = buildBoardModel({ view, query: "spill" });
    const ids = [
      ...filtered.columns.flatMap((column) => column.cards),
      ...filtered.archived,
    ].map((card) => card.id);
    expect(ids.sort()).toEqual([alpha.id, archived.id].sort());

    // The SE key is searchable too.
    const byKey = buildBoardModel({ view, query: alpha.displayKey });
    expect(byKey.columns[0].cards.map((card) => card.id)).toEqual([alpha.id]);

    // Empty model on a miss, still exactly three columns.
    const miss = buildBoardModel({ view, query: "zzz-no-match" });
    expect(miss.columns.map((column) => column.cards.length)).toEqual([0, 0, 0]);
    expect(miss.archived).toEqual([]);
  });
});

describe("empty view", () => {
  it("builds three empty columns for a null view (still loading)", () => {
    const model = buildBoardModel({ view: null });
    expect(model.columns).toHaveLength(3);
    expect(model.total).toBe(0);
    expect(model.byId.size).toBe(0);
  });
});
