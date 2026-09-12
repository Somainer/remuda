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

/** Registry key is driverKind + '.' + nativeToolName, then mapped to a family. */
export function registryKey(driverKind: string, toolName: string): string {
  return `${driverKind}.${toolName}`;
}

export function familyFor(driverKind: string, toolName: string | undefined | null): ToolFamily {
  if (!toolName) return "Generic";
  const key = registryKey(driverKind, toolName);
  const mapped = FAMILY_BY_NAME[key.split(".").slice(1).join(".")];
  if (mapped) return mapped;
  if (toolName.startsWith("mcp__") || toolName.includes("__")) return "MCP";
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

export const TOOL_FAMILIES: ToolFamily[] = ["Bash", "Edit", "Read", "Write", "Workflow", "Task", "MCP", "Generic"];
