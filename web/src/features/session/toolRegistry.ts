export type ToolFamily = "Bash" | "Edit" | "Read" | "Write" | "Workflow" | "Task" | "MCP" | "Generic";

const FAMILY_BY_NAME: Record<string, ToolFamily> = {
  Bash: "Bash",
  Edit: "Edit",
  Read: "Read",
  Write: "Write",
  Workflow: "Workflow",
  Task: "Task",
  Agent: "Task",
};

/**
 * Grok native tool names → card family. This is the TS half of the table in
 * docs/design/grok-structural-translation.md §3.1; the Rust half is the
 * name→ToolCategory table in grok_adapter.rs — keep both in sync.
 *
 * Every file adapter (grok/codex) stamps driverKind `shell-pty`, so the
 * native name alone carries the harness identity. The registry key stays the
 * native name: grok tools are rendered with the shared shell/read/write/task
 * layouts, never renamed to Claude tool names.
 */
const GROK_FAMILY_BY_NAME: Record<string, ToolFamily> = {
  run_terminal_command: "Bash",
  read_file: "Read",
  list_dir: "Read",
  write: "Write",
  search_replace: "Edit",
  // No Search family exists yet: search tools use the Generic card in a
  // search-styled presenter (design §3.1).
  grep: "Generic",
  web_search: "Generic",
  web_fetch: "Generic",
  open_page: "Generic",
  open_page_with_find: "Generic",
  spawn_subagent: "Task",
  workflow: "Workflow",
  // Grok qualified MCP names are `server__tool`, but they arrive inside
  // `use_tool.tool_name`; the bare qualified string is never guessed as MCP.
  search_tool: "MCP",
  use_tool: "MCP",
  // Normally projected to an interaction (QuestionForm); the card is the
  // fallback for a frame without one.
  ask_user_question: "Generic",
};

function grokFamilyFor(toolName: string): ToolFamily | null {
  const exact = GROK_FAMILY_BY_NAME[toolName];
  if (exact) return exact;
  // Prefix families from design §3.1: open_page / open_page_with_find and the
  // whole x_* search family.
  if (toolName.startsWith("open_page") || toolName.startsWith("x_")) return "Generic";
  return null;
}

/**
 * True for a grok native tool name. The dedicated family cards use this to
 * pick grok input fields (`target_file`, `use_tool.tool_name`, …) and the
 * human display title instead of the Claude-shaped defaults.
 */
export function isGrokTool(toolName: string | undefined | null): boolean {
  return Boolean(toolName && grokFamilyFor(toolName) !== null);
}

/** Registry key is driverKind + '.' + nativeToolName, then mapped to a family. */
export function registryKey(driverKind: string, toolName: string): string {
  return `${driverKind}.${toolName}`;
}

export function familyFor(driverKind: string, toolName: string | undefined | null): ToolFamily {
  if (!toolName) return "Generic";
  const grok = grokFamilyFor(toolName);
  if (grok) return grok;
  const key = registryKey(driverKind, toolName);
  const mapped = FAMILY_BY_NAME[key.split(".").slice(1).join(".")];
  if (mapped) return mapped;
  // Only a real MCP qualified name (`mcp__server__tool`) or an explicit MCP
  // family in the table above maps to MCP. A bare `__` inside an unknown name
  // no longer does — grok's own `server__tool` qualified names are read from
  // the `use_tool` input, and an unrelated name with underscores is Generic.
  if (toolName.startsWith("mcp__")) return "MCP";
  if (FAMILY_BY_NAME[toolName]) return FAMILY_BY_NAME[toolName];
  return "Generic";
}

export function splitMcpName(toolName: string): { server: string; tool: string } {
  if (toolName.startsWith("mcp__")) {
    const rest = toolName.slice(5);
    const idx = rest.lastIndexOf("__");
    if (idx >= 0) return { server: rest.slice(0, idx), tool: rest.slice(idx + 2) };
  }
  return { server: "mcp", tool: toolName };
}

/**
 * Split a grok qualified tool name (`server__tool`, no `mcp__` prefix). The
 * separator is the first `__`; an unqualified name belongs to the grok_build
 * namespace. Used only for the explicit `use_tool` / `search_tool` calls.
 */
export function splitGrokMcpName(toolName: string): { server: string; tool: string } {
  const idx = toolName.indexOf("__");
  if (idx > 0) return { server: toolName.slice(0, idx), tool: toolName.slice(idx + 2) };
  return { server: "grok_build", tool: toolName };
}

export const TOOL_FAMILIES: ToolFamily[] = ["Bash", "Edit", "Read", "Write", "Workflow", "Task", "MCP", "Generic"];
