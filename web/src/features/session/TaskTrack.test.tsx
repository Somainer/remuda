import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import type { ToolNode } from "./assemble";
import { TaskTrack } from "./TaskTrack";

function task(
  idVal: string,
  prompt: string,
  outcome?: "succeeded" | "failed" | "denied",
  stage: "final" | "partial" = "final",
): ToolNode {
  return {
    type: "tool",
    id: idVal,
    family: "Task",
    name: "Task",
    driverKind: "claude-print",
    call: {
      nodeId: `n-${idVal}` as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      toolCallId: idVal as Id,
      parentToolCallId: null,
      toolName: known("Task"),
      displayTitle: known("Task"),
      category: "agent",
      input: known({ prompt }),
      inputTextDelta: null,
      state: "running",
      executor: known({ hostId: "h" as Id, workspaceId: null, nativeAgentId: null }),
    },
    result:
      outcome === undefined
        ? null
        : {
            nodeId: `r-${idVal}` as Id,
            revision: "1",
            operation: "close",
            baseRevision: null,
            toolCallId: idVal as Id,
            stage,
            outcome,
            blocks: [],
            structuredResult: unknownKnowledge("text"),
            exitCode: known(outcome === "succeeded" ? 0 : 1),
            changes: [],
          },
    completeness: "structured",
    diffState: "unknown",
  };
}

describe("TaskTrack", () => {
  it("renders nothing without tasks", () => {
    const { container } = render(<TaskTrack tasks={[]} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("shows a short prompt in full with no expand toggle", () => {
    render(<TaskTrack tasks={[task("t1", "short prompt")]} />);
    expect(screen.getByTestId("task-prompt-text")).toHaveTextContent("short prompt");
    expect(screen.queryByTestId("task-prompt-toggle")).toBeNull();
  });

  it("truncates a long prompt and expands/collapses it on demand", async () => {
    const user = userEvent.setup();
    const long = "重构整个会话组装层，".repeat(20);
    render(<TaskTrack tasks={[task("t1", long)]} />);
    const text = screen.getByTestId("task-prompt-text");
    const toggle = screen.getByTestId("task-prompt-toggle");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(text.textContent!.length).toBeLessThan(long.length);
    expect(text.textContent!.endsWith("…")).toBe(true);
    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("task-prompt-text")).toHaveTextContent(long);
    await user.click(toggle);
    expect(screen.getByTestId("task-prompt-text").textContent!.endsWith("…")).toBe(true);
  });

  it("labels running, succeeded and failed tasks distinctly", () => {
    const { container } = render(
      <TaskTrack
        tasks={[task("run", "a"), task("ok", "b", "succeeded"), task("bad", "c", "failed"), task("deny", "d", "denied")]}
      />,
    );
    const items = screen.getAllByTestId("task-track-item");
    expect(items.map((i) => i.getAttribute("data-task-outcome"))).toEqual([
      "running",
      "succeeded",
      "failed",
      "denied",
    ]);
    expect(items[2]?.textContent).toContain("failed");
    expect(items[3]?.textContent).toContain("denied");
    // Failed outcomes take the warning class so they read as problems.
    expect(container.querySelector('[data-task-outcome="failed"] span:last-child')?.className).not.toHaveLength(0);
  });

  it("marks a launched background subagent as running in background until its final result", () => {
    // Launch returns immediately with a partial result (agentId); completion
    // folds a final result later. The row must read "running in background"
    // in between, never "succeeded".
    const { rerender } = render(
      <TaskTrack tasks={[task("bg", "background work", "succeeded", "partial")]} />,
    );
    let item = screen.getByTestId("task-track-item");
    expect(item.getAttribute("data-task-outcome")).toBe("background");
    expect(item.textContent).toContain("running in background");
    expect(item.textContent).not.toContain("succeeded");

    rerender(<TaskTrack tasks={[task("bg", "background work", "succeeded", "final")]} />);
    item = screen.getByTestId("task-track-item");
    expect(item.getAttribute("data-task-outcome")).toBe("succeeded");
    expect(item.textContent).not.toContain("running in background");
  });
});
