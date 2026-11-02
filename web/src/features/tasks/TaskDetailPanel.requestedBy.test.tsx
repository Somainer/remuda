import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";
import type { Task } from "../../types/generated";
import { TaskDetailPanel } from "./TaskDetailPanel";

function task(patch: Partial<Task> = {}): Task {
  return {
    id: "tsk_requested_by",
    revision: "1",
    createdAt: "2026-09-20T10:00:00.000Z",
    updatedAt: "2026-09-20T10:00:00.000Z",
    projectId: "prj_requested_by",
    title: "Ship the thing",
    mandate: { chain: [] },
    class: "implement",
    state: "pending",
    ...patch,
  } as Task;
}

function renderPanel(row: Task | null) {
  return render(
    <MemoryRouter>
      <TaskDetailPanel task={row} />
    </MemoryRouter>,
  );
}

describe("TaskDetailPanel requestedBy (cb-requested-by)", () => {
  it("shows the owner's own words when the Agent acted in an owner turn", () => {
    renderPanel(
      task({
        requestedBy: {
          commandId: "cmd_owner",
          excerpt: "dispatch a worker on the other node",
        },
      }),
    );
    expect(screen.getByTestId("task-detail-requested-by")).toBeInTheDocument();
    expect(screen.getByTestId("requested-by")).toHaveTextContent(
      "你说：dispatch a worker on the other node",
    );
    // An owner turn carries no "agent 自发" tag.
    expect(screen.queryByTestId("requested-by-tag")).toBeNull();
  });

  it("labels a task an Agent created on its own turn", () => {
    renderPanel(task({ requestedBy: { origin: "agent" } }));
    expect(screen.getByTestId("requested-by-tag")).toHaveTextContent("agent 自发");
    expect(screen.queryByTestId("requested-by")).toBeNull();
  });

  it("labels a task created in a hub-origin turn as agent 自发", () => {
    renderPanel(task({ requestedBy: { origin: "hub" } }));
    expect(screen.getByTestId("requested-by-tag")).toHaveTextContent("agent 自发");
  });

  it("renders nothing when the Hub recorded no requester", () => {
    renderPanel(task());
    expect(screen.queryByTestId("task-detail-requested-by")).toBeNull();
  });
});
