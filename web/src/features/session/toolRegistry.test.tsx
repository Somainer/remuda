import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
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

/** A grok call: stable native name, human ACP title, arbitrary native input. */
function grokCall(name: string, title: string, input: unknown): ToolCallPayload {
  return {
    ...call(name, {}),
    displayTitle: known(title),
    input: known(input),
  };
}

function grokResult(structured: unknown): ToolResultPayload {
  return {
    nodeId: "obj_n" as Id,
    revision: "2",
    operation: "replace",
    baseRevision: "1",
    toolCallId: "obj_c" as Id,
    stage: "final",
    outcome: "succeeded",
    blocks: [],
    structuredResult: known(structured),
    exitCode: known(0),
    changes: [],
  };
}

function renderGrok(cardCall: ToolCallPayload, result: ToolResultPayload | null = null) {
  return render(
    <ToolCard
      driverKind="shell-pty"
      call={cardCall}
      result={result}
      completeness="structured"
      diffState="unknown"
    />,
  );
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

  it("renders the Bash card for Bash, and a key/value summary for unknown tools", () => {    const { rerender } = render(
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

/**
 * Rendered grok cards (driverKind shell-pty). These catch the wiring bugs the
 * pure presenter tests cannot: the family-specific cards (BashCard, ReadCard,
 * McpCard) had to learn the grok input fields, and dispatch uses the native
 * name rather than the display title. Fixture inputs from
 * crates/remuda-driver/tests/fixtures/grok/tui-updates.jsonl lines 8/37; the
 * read_file/use_tool input shapes are synthesized [U].
 */
describe("rendered grok ToolCards", () => {
  it("run_terminal_command: human title heading, native label, command, no JSON dump", () => {
    // Input/title verbatim from fixture line 8.
    const { container } = renderGrok(
      grokCall("run_terminal_command", "Execute `printf SPIKE_OK > x.txt`", {
        variant: "Bash",
        command: "printf SPIKE_OK > x.txt",
        description: "Write the marker.",
        is_background: false,
      }),
    );
    expect(screen.getByText("Execute `printf SPIKE_OK > x.txt`")).toBeTruthy();
    expect(screen.getByTestId("tool-native-name").textContent).toBe("run_terminal_command");
    expect(screen.getByText("$ printf SPIKE_OK > x.txt")).toBeTruthy();
    // The native input keys never reach the card as a key/value dump.
    expect(screen.queryByText("is_background")).toBeNull();
    expect(container.textContent).not.toContain("variant");
  });

  it("run_terminal_command: cwd comes from the completed frame's rawOutput (line 9)", () => {
    renderGrok(
      grokCall("run_terminal_command", "Execute `printf x`", { command: "printf x" }),
      grokResult({ status: "completed", rawOutput: { exit_code: 0, current_dir: "/workspace/grok-spike" } }),
    );
    expect(screen.getByText("/workspace/grok-spike")).toBeTruthy();
  });

  it("read_file: target_file with the offset/limit range, not a literal 'file'", () => {
    // Synthesized from docs, not captured [U].
    const { container } = renderGrok(
      grokCall("read_file", "Read main.rs", { target_file: "/repo/src/main.rs", offset: 10, limit: 40 }),
    );
    expect(screen.getByText("Read main.rs")).toBeTruthy();
    expect(screen.getByTestId("tool-native-name").textContent).toBe("read_file");
    expect(screen.getByText("/repo/src/main.rs:10-40")).toBeTruthy();
    expect(screen.queryByText("file")).toBeNull();
    expect(screen.queryByText("target_file")).toBeNull();
    expect(container.textContent).not.toContain("offset");
  });

  it("list_dir: target_directory renders (Read family, grok fields)", () => {
    // Synthesized from docs, not captured [U].
    renderGrok(grokCall("list_dir", "List src", { target_directory: "/repo/src" }));
    expect(screen.getByText("/repo/src")).toBeTruthy();
    expect(screen.getByTestId("tool-native-name").textContent).toBe("list_dir");
    expect(screen.queryByText("target_directory")).toBeNull();
  });

  it("ask_user_question: question + option labels, no raw questions[] JSON", () => {
    // Input/title verbatim from fixture line 37.
    const { container } = renderGrok(
      grokCall("ask_user_question", "Ask: Choose the probe result.", {
        variant: "AskUserQuestion",
        questions: [
          {
            question: "Choose the probe result.",
            options: [
              { label: "Alpha", description: "Record Alpha." },
              { label: "Beta", description: "Record Beta." },
            ],
            multiSelect: null,
          },
        ],
      }),
    );
    expect(screen.getByText("Ask: Choose the probe result.")).toBeTruthy();
    expect(screen.getByTestId("tool-native-name").textContent).toBe("ask_user_question");
    expect(screen.getByText("Choose the probe result.")).toBeTruthy();
    expect(screen.getByText("Alpha / Beta")).toBeTruthy();
    expect(screen.queryByText("questions")).toBeNull();
    expect(container.textContent).not.toContain("Record Alpha");
  });

  it("use_tool: qualified server/tool from tool_name, not a 'mcp' server", () => {
    // Synthesized from docs, not captured [U].
    const { container } = renderGrok(
      grokCall("use_tool", "Search drive", { tool_name: "drive__search_files", query: "spec" }),
    );
    expect(screen.getByText("drive/search_files")).toBeTruthy();
    expect(screen.getByTestId("tool-native-name").textContent).toBe("use_tool");
    expect(screen.queryByText("mcp/use_tool")).toBeNull();
    expect(container.textContent).not.toContain("mcp/use_tool");
  });
});
