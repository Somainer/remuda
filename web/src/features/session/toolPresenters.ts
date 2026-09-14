/**
 * Per-tool presenters for the 结构 view (D-028 P3 feedback A).
 *
 * The user compared Remuda's transcript with Claude Code's own TUI: a
 * `Workflow` call rendered as an empty card reading "Workflow running", and
 * `TaskOutput` dumped its raw JSON input. Claude's TUI shows
 * `Workflow(<description>)` and `Task Output <task_id>` with a human sentence
 * underneath.
 *
 * A presenter turns one tool call (and its result, once it lands) into a
 * title, a subtitle, and labelled detail rows. Everything here is pure so the
 * parsing is unit-testable without rendering; `ToolCard` does the drawing.
 *
 * Unknown tools fall back to a compact key/value summary rather than a raw
 * JSON dump — the raw payload stays one toggle away, never the default.
 */
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString } from "../../lib/format";
import { splitMcpName } from "./toolRegistry";

/** One labelled row in a tool card body. */
export type ToolDetail = {
  label: string;
  value: string;
  /** Render in a monospace block rather than inline. */
  pre?: boolean;
  /** Start collapsed behind a disclosure. */
  fold?: boolean;
};

export type ToolPresentation = {
  /** Card heading, e.g. `Workflow`. */
  title: string;
  /** The one line that says what this call is doing. */
  subtitle: string | null;
  /** Short status word: `running` until the result lands, then done/failed. */
  status: "running" | "done" | "failed";
  /** Body rows. */
  details: ToolDetail[];
};

/** Text blocks of a result, joined. */
export function resultText(result: ToolResultPayload | null): string {
  if (!result) return "";
  return result.blocks
    .map((block) => (block.type === "text" ? block.text : ""))
    .filter(Boolean)
    .join("\n");
}

function status(result: ToolResultPayload | null): ToolPresentation["status"] {
  if (!result) return "running";
  return result.outcome === "failed" || result.outcome === "denied" ? "failed" : "done";
}

/**
 * Pull `name` / `description` out of a Workflow script's `export const meta`.
 *
 * The tool's only input is the script source, so the title has to come from
 * the literal inside it. Deliberately tolerant: a regex over the source rather
 * than a JS parse, because a script that does not match must degrade to the
 * `description` input, never throw and blank the card.
 */
export function parseWorkflowMeta(script: string): { name?: string; description?: string } {
  const meta = /export\s+const\s+meta\s*=\s*\{/.exec(script);
  if (!meta) return {};
  // Scan to the matching brace so a `phases: [{...}]` inside cannot end it early.
  let depth = 0;
  let end = -1;
  for (let i = meta.index + meta[0].length - 1; i < script.length; i++) {
    const ch = script[i];
    if (ch === "{") depth += 1;
    else if (ch === "}") {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }
  const body = script.slice(meta.index, end < 0 ? script.length : end + 1);
  const field = (key: string): string | undefined => {
    const match = new RegExp(`\\b${key}\\s*:\\s*(['"\`])([\\s\\S]*?)\\1`).exec(body);
    return match?.[2];
  };
  return { name: field("name"), description: field("description") };
}

/** Minutes/seconds in words, for a timeout given in milliseconds. */
export function humanDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "未设置";
  if (ms < 1000) return `${Math.round(ms)} 毫秒`;
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.round(seconds / 60);
  return `${minutes} 分钟`;
}

/** A compact `key: value` summary — the unknown-tool fallback. */
export function compactInput(input: unknown): ToolDetail[] {
  const record = asRecord(input);
  if (!record) {
    const text = typeof input === "string" ? input : input === undefined ? "" : JSON.stringify(input);
    return text ? [{ label: "输入", value: text, pre: text.includes("\n") }] : [];
  }
  return Object.entries(record).map(([key, value]) => {
    const text =
      typeof value === "string"
        ? value
        : value === null || value === undefined
          ? "—"
          : JSON.stringify(value);
    return { label: key, value: text, pre: text.length > 80 || text.includes("\n") };
  });
}

function withResult(details: ToolDetail[], result: ToolResultPayload | null, label = "结果"): ToolDetail[] {
  const text = resultText(result);
  if (!text) return details;
  return details.concat({ label, value: text, pre: true, fold: text.split("\n").length > 3 });
}

/**
 * Build the presentation for one tool call.
 *
 * `name` is the native tool name; the registry family is not enough because
 * `Workflow` and `TaskOutput` need their own shapes while sharing a family
 * with nothing else.
 */
export function presentTool(
  name: string,
  call: ToolCallPayload,
  result: ToolResultPayload | null,
): ToolPresentation {
  const input = knowledgeValue(call.input);
  const record = asRecord(input);
  const state = status(result);

  if (name === "Workflow") {
    const script = asString(record?.script) ?? "";
    const meta = parseWorkflowMeta(script);
    // Claude's TUI titles the card with the description, so match it; the
    // `description` input is the fallback when the script has no meta literal.
    const subtitle = meta.description ?? asString(record?.description) ?? meta.name ?? null;
    const details: ToolDetail[] = [];
    if (meta.name) details.push({ label: "工作流", value: meta.name });
    if (state === "running") {
      details.push({ label: "状态", value: "在后台运行 · /workflows 可查看进度" });
    }
    if (script) {
      details.push({ label: "脚本", value: script, pre: true, fold: true });
    }
    return { title: "Workflow", subtitle, status: state, details: withResult(details, result) };
  }

  if (name === "TaskOutput") {
    const taskId = asString(record?.task_id) ?? asString(record?.taskId) ?? "?";
    const block = record?.block;
    const timeout = typeof record?.timeout === "number" ? record.timeout : undefined;
    const waiting =
      block === false
        ? `查询任务 ${taskId} 的当前状态`
        : `等待任务 ${taskId}${timeout ? `，最长 ${humanDuration(timeout)}` : ""}`;
    const details: ToolDetail[] = [{ label: "任务", value: taskId }];
    details.push({ label: state === "running" ? "等待中" : "等待", value: waiting });
    return {
      title: "Task Output",
      subtitle: waiting,
      status: state,
      details: withResult(details, result, "输出"),
    };
  }

  if (name === "Agent" || name === "Task") {
    const description = asString(record?.description) ?? asString(record?.prompt) ?? null;
    const type = asString(record?.subagent_type) ?? asString(record?.subagentType);
    const details: ToolDetail[] = [];
    if (type) details.push({ label: "子代理", value: type });
    const prompt = asString(record?.prompt);
    if (prompt) details.push({ label: "任务", value: prompt, pre: true, fold: prompt.length > 200 });
    return {
      title: "Task",
      subtitle: description,
      status: state,
      details: withResult(details, result),
    };
  }

  if (name === "Bash") {
    const command = asString(record?.command) ?? "";
    const details: ToolDetail[] = [{ label: "$", value: command, pre: true }];
    const cwd = asString(record?.cwd);
    if (cwd) details.push({ label: "目录", value: cwd });
    const exit = result ? knowledgeValue(result.exitCode) : undefined;
    if (exit !== undefined && exit !== null) details.push({ label: "exit", value: String(exit) });
    return {
      title: "Bash",
      subtitle: asString(record?.description) ?? (command.split("\n")[0] || null),
      status: state,
      details: withResult(details, result, "输出"),
    };
  }

  if (name === "Read" || name === "Edit" || name === "Write" || name === "NotebookEdit") {
    const path = asString(record?.file_path) ?? asString(record?.notebook_path) ?? result?.changes[0]?.path ?? "file";
    const details: ToolDetail[] = [{ label: "路径", value: path }];
    const offset = record?.offset;
    const limit = record?.limit;
    if (typeof offset === "number" || typeof limit === "number") {
      details.push({ label: "范围", value: `第 ${String(offset ?? 1)} 行起${limit ? `，${String(limit)} 行` : ""}` });
    }
    const changed = result?.changes.length ?? 0;
    if (changed) details.push({ label: "改动", value: `${changed} 处` });
    return { title: name, subtitle: path, status: state, details };
  }

  if (name.startsWith("mcp__") || name.includes("__")) {
    const { server, tool } = splitMcpName(name);
    return {
      title: "MCP",
      subtitle: `${server}/${tool}`,
      status: state,
      details: withResult(compactInput(input), result),
    };
  }

  // Unknown tool: a readable key/value summary, not a JSON dump. The raw
  // payload stays available behind the card's 原始 toggle.
  return {
    title: knowledgeValue(call.displayTitle) ?? name,
    subtitle: null,
    status: state,
    details: withResult(compactInput(input), result),
  };
}
