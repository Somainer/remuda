import { render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { TaskGroups } from "./TaskList";
import type { TaskListGroup, TaskRow } from "./taskRows";
import css from "./tasklist.module.css";

/**
 * c-perffu r2 item 2: the memoized rail row's comparator used to consider
 * only its OWN selected state. When the selection was a descendant, moving
 * it elsewhere left the old child painted selected (its memoized parent
 * skipped re-render and never handed it the new selectedId), so two rows
 * could look selected at once.
 */

function row(id: string, children: TaskRow[] = []): TaskRow {
  return {
    task: { id, title: id } as TaskRow["task"],
    id,
    displayKey: id.toUpperCase(),
    depth: 0,
    childCount: children.length,
    sessionIds: [],
    sessionCount: 0,
    primarySessionId: null,
    spaceId: null,
    branch: null,
    needsHuman: false,
    blocked: false,
    children,
  };
}

// parent → child (nested two levels), plus an unrelated top-level row.
const child = row("child");
child.depth = 1;
const parent = row("parent", [child]);
const other = row("other");
const groups: TaskListGroup[] = [
  {
    id: "g1",
    kind: "project",
    projectId: "prj",
    spaceId: null,
    project: "prj",
    branch: null,
    blockedCount: 0,
    count: 2,
    rows: [parent],
  },
  {
    id: "g2",
    kind: "project",
    projectId: "prj2",
    spaceId: null,
    project: "prj2",
    branch: null,
    blockedCount: 0,
    count: 1,
    rows: [other],
  },
];

function selectedIds(container: HTMLElement): string[] {
  return [...container.querySelectorAll<HTMLElement>("[data-testid='task-row']")]
    .filter((el) => el.className.includes(css.rowSelected))
    .map((el) => el.dataset.taskId ?? "");
}

function renderRail(selectedId: string | null) {
  return render(
    <MemoryRouter>
      <TaskGroups
        groups={groups}
        variant="desktop"
        selectedId={selectedId}
        onSelect={onSelect}
      />
    </MemoryRouter>,
  );
}

// Stable across rerenders: a fresh callback identity would bypass the row
// memo and invalidate this regression.
const onSelect = vi.fn();

describe("TaskGroups nested selection", () => {
  it("repaints when selection moves from a child to a top-level row and when cleared — never two selected", () => {
    // 1. Select the child.
    const { container, rerender } = renderRail("child");
    expect(selectedIds(container)).toEqual(["child"]);

    // 2. Move to the unrelated top-level row: the child must clear.
    rerender(
      <MemoryRouter>
        <TaskGroups groups={groups} variant="desktop" selectedId="other" onSelect={onSelect} />
      </MemoryRouter>,
    );
    expect(selectedIds(container)).toEqual(["other"]);

    // 3. Select the parent itself, then the child again (move WITHIN the same
    //    parent subtree): only one row painted at each step.
    rerender(
      <MemoryRouter>
        <TaskGroups groups={groups} variant="desktop" selectedId="parent" onSelect={onSelect} />
      </MemoryRouter>,
    );
    expect(selectedIds(container)).toEqual(["parent"]);
    rerender(
      <MemoryRouter>
        <TaskGroups groups={groups} variant="desktop" selectedId="child" onSelect={onSelect} />
      </MemoryRouter>,
    );
    expect(selectedIds(container)).toEqual(["child"]);

    // 4. Clear (Esc closes the preview): nothing stays selected.
    rerender(
      <MemoryRouter>
        <TaskGroups groups={groups} variant="desktop" selectedId={null} onSelect={onSelect} />
      </MemoryRouter>,
    );
    expect(selectedIds(container)).toEqual([]);
  });
});
