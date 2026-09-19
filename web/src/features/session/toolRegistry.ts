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

/**
 * Native tool names whose call IS an interaction (ui-spec §2.2, D-041).
 *
 * These are normally projected onto `interaction.requested` and rendered as
 * QuestionForm pinned above the composer, so no ToolCard mounts for them. The
 * names below cover the fallback card — a tool-only frame the projection did
 * not carry — which must stay open the same way: a pending question is never
 * swept behind a fold.
 */
const INTERACTION_TOOL_NAMES = new Set(["AskUserQuestion", "ask_user_question"]);

export function isInteractionTool(toolName: string | undefined | null): boolean {
  return Boolean(toolName && INTERACTION_TOOL_NAMES.has(toolName));
}

/**
 * Inputs to the D-041 compact default-fold decision (ui-spec.md §2.2 fold
 * table). The decision is made AFTER the family is determined, so a fold
 * branch can never swallow a family-specific card (the Workflow timeline).
 */
export type ToolFoldInput = {
  /** Card family from {@link familyFor}. */
  family: ToolFamily;
  /** Call/result paired (final result), or the card explicitly failed. */
  settled: boolean;
  /** Compact workbench layout. The automatic default fold applies only here. */
  compact: boolean;
  /** result.outcome is failed/denied — error cards always stay open. */
  failed?: boolean;
  /** interaction.* fallback card — stays pinned above the composer. */
  interaction?: boolean;
  /** Reader explicitly pressed 全部折叠 (collapse-all). Folds every non-failed card. */
  requested?: boolean;
};

/**
 * Whether a tool card starts (and stays, until the reader expands it) folded
 * to its one-line row. D-041, ui-spec.md §2.2:
 *
 * - an explicit collapse-all (`requested`) keeps main's exact behaviour:
 *   every non-failed card folds, including running and Workflow cards —
 *   D-041 exemptions govern only the AUTOMATIC compact fold;
 * - the automatic fold applies only in compact layout;
 * - Workflow, error and interaction.* cards are exempt there, no matter
 *   how long they have been settled;
 * - running / unsettled cards never fold automatically — but a card that
 *   settles during a live session folds the moment it settles: a live phone
 *   session is the scroll problem D-041 exists for.
 */
export function shouldFoldToolCard(input: ToolFoldInput): boolean {
  if (input.requested) return !input.failed;
  if (!input.compact) return false;
  if (input.failed || input.interaction || input.family === "Workflow") return false;
  return input.settled;
}
