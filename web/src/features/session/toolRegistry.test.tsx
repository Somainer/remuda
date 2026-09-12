import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ToolCallPayload } from "../../types/observation";
import { known, type Id } from "../../types/wire";
import { familyFor, registryKey, TOOL_FAMILIES } from "./toolRegistry";
import { ToolCard } from "./ToolCard";

function call(toolName: string, input: Record<string, string>): ToolCallPayload {
  return {
    nodeId: "obj_n" as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: "obj_c" as Id,
    parentToolCallId: null,
    toolName: known(toolName),
    displayTitle: known(toolName),
    category: "other",
    input: known(input),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
  };
}

describe("tool registry dispatch", () => {
  it("maps native names onto families and does not call Codex commandExecution Bash", () => {
    expect(familyFor("claude-print", "Bash")).toBe("Bash");
    expect(familyFor("claude-print", "Edit")).toBe("Edit");
    expect(familyFor("claude-print", "Read")).toBe("Read");
    expect(familyFor("claude-print", "Write")).toBe("Write");
    expect(familyFor("claude-print", "Workflow")).toBe("Workflow");
    expect(familyFor("claude-print", "Task")).toBe("Task");
    expect(familyFor("claude-print", "Agent")).toBe("Task");
    expect(familyFor("claude-print", "mcp__claude_ai_Google_Drive__search_files")).toBe("MCP");
    expect(familyFor("claude-print", "Mystery")).toBe("Generic");
    expect(familyFor("codex-appserver", "commandExecution")).toBe("Generic");
    expect(registryKey("claude-print", "Bash")).toBe("claude-print.Bash");
    expect(TOOL_FAMILIES).toContain("Generic");
  });

  it("renders the Bash card for Bash and Generic for unknown tools", () => {
    const { rerender } = render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "ninja -C build" })}
        result={null}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByText("Bash")).toBeTruthy();
    expect(screen.getByText(/ninja -C build/)).toBeTruthy();
    rerender(
      <ToolCard
        driverKind="claude-print"
        call={call("NotARealTool", { foo: "bar" })}
        result={null}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByText("Generic")).toBeTruthy();
    expect(screen.getByText("NotARealTool")).toBeTruthy();
  });
});
