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

  it("maps grok native names to families while keeping the native key", () => {
    // docs/design/grok-structural-translation.md §3.1. File adapters stamp
    // driverKind shell-pty; the name alone carries the identity.
    expect(familyFor("shell-pty", "run_terminal_command")).toBe("Bash");
    expect(familyFor("shell-pty", "read_file")).toBe("Read");
    expect(familyFor("shell-pty", "list_dir")).toBe("Read");
    expect(familyFor("shell-pty", "write")).toBe("Write");
    expect(familyFor("shell-pty", "search_replace")).toBe("Edit");
    expect(familyFor("shell-pty", "grep")).toBe("Generic");
    expect(familyFor("shell-pty", "web_search")).toBe("Generic");
    expect(familyFor("shell-pty", "web_fetch")).toBe("Generic");
    expect(familyFor("shell-pty", "open_page")).toBe("Generic");
    expect(familyFor("shell-pty", "open_page_with_find")).toBe("Generic");
    expect(familyFor("shell-pty", "x_post_timeline")).toBe("Generic");
    expect(familyFor("shell-pty", "spawn_subagent")).toBe("Task");
    expect(familyFor("shell-pty", "workflow")).toBe("Workflow");
    expect(familyFor("shell-pty", "search_tool")).toBe("MCP");
    expect(familyFor("shell-pty", "use_tool")).toBe("MCP");
    expect(familyFor("shell-pty", "ask_user_question")).toBe("Generic");
    // The registry key is still the native name, never a Claude alias.
    expect(registryKey("shell-pty", "run_terminal_command")).toBe("shell-pty.run_terminal_command");
  });

  it("no longer treats a bare double underscore in an unknown name as MCP", () => {
    // grok qualified names are `server__tool`, but they arrive inside
    // use_tool.tool_name; a bare `__` must not label some other unknown tool.
    expect(familyFor("shell-pty", "drive__search_files")).toBe("Generic");
    expect(familyFor("shell-pty", "plan__draft__v2")).toBe("Generic");
    // The real Claude qualified spelling still maps.
    expect(familyFor("claude-print", "mcp__server__tool")).toBe("MCP");
  });

  it("renders the Bash card for Bash, and a key/value summary for unknown tools", () => {
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
    // An unknown tool gets a readable key/value summary rather than the raw
    // JSON dump the old Generic card printed (D-028 P3 feedback A).
    expect(screen.getByText("NotARealTool")).toBeTruthy();
    expect(screen.getByText("foo")).toBeTruthy();
    expect(screen.getByText("bar")).toBeTruthy();
  });
});
