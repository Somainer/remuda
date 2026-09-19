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
import { splitGrokMcpName, splitMcpName } from "./toolRegistry";

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

/** One image a tool result staged in the Hub object store (D-045 §6.2). */
export type ResultMedia = {
  /** `obj_…` id the bytes are served from at `/v1/objects/{id}`. */
  objectId: string;
  /** Stored media type, e.g. `image/png`. */
  mediaType: string;
  /** Block name, used as the thumbnail's alt text. */
  name: string;
};

/**
 * Image blocks of a result, in block order.
 *
 * Text stays with {@link resultText}; an image that could not be staged is a
 * text block on the producer side, so this never needs a broken-image branch
 * (ui-spec §2.2).
 */
export function resultMedia(result: ToolResultPayload | null): ResultMedia[] {
  if (!result) return [];
  return result.blocks.flatMap((block) =>
    block.type === "image"
      ? [{ objectId: block.objectId, mediaType: block.mediaType, name: block.name ?? "image" }]
      : [],
  );
}

function status(result: ToolResultPayload | null): ToolPresentation["status"] {
  if (!result) return "running";
  return result.outcome === "failed" || result.outcome === "denied" ? "failed" : "done";
}

/**
 * Pull `name` / `description` out of a workflow script's meta literal.
 *
 * The tool's only input is the script source, so the title has to come from
 * the literal inside it. Deliberately tolerant: a regex over the source rather
 * than a JS parse, because a script that does not match must degrade to the
 * `description` input, never throw and blank the card.
 *
 * Two literal dialects share the brace scan: Claude's
 * `export const meta = {` (default) and grok's Rhai `let meta = #{`.
 */
export function parseWorkflowMeta(
  script: string,
  opts: { opener?: RegExp } = {},
): { name?: string; description?: string } {
  const meta = (opts.opener ?? /export\s+const\s+meta\s*=\s*\{/).exec(script);
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

/** Grok Rhai workflow meta: `let meta = #{ name: "…", description: "…" };`. */
export const RHAI_META_OPENER = /let\s+meta\s*=\s*#\{/;

/** Truncate a long script for the no-meta fallback, keeping the card honest. */
function truncate(text: string, max = 200): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max)}…`;
}

/** First physical line of a command — the folded row's key argument (D-041). */
function firstLine(text: string | null | undefined): string | null {
  if (!text) return null;
  const line = text.split("\n")[0];
  return line.length ? line : null;
}

/**
 * Single-line JSON preview for a command-less Bash input. `jsonPreview` is a
 * pretty-printed (multi-line, brace-first) dump made for raw panels; the
 * folded row must stay one physical line, so compact it to one JSON string.
 */
function singleLinePreview(input: unknown): string {
  if (typeof input === "string") return input;
  if (input === undefined) return "";
  try {
    return JSON.stringify(input);
  } catch {
    return String(input);
  }
}

/**
 * The key argument a folded one-line row carries next to its family.
 */
export type FoldedKeyArgument = {
  /** What the one-line row shows — the command's first physical line. */
  text: string;
  /** The full value (all command lines / the whole path) for the `title`. */
  title: string;
};

/**
 * The key argument a folded one-line row must carry next to its family
 * (D-041, ui-spec.md §2.2): the Bash command's first line, or the path for
 * Edit / Write / Read (Claude and grok field names). Returns null for
 * families the spec table gives no key argument (Task / MCP / Generic); the
 * caller then renders family alone, except a Bash call whose input has no
 * `command` falls back to a JSON preview so the row is never a bare `Bash`.
 *
 * The row shows `text` and truncates by width; `title` carries the complete
 * value (`text` is only the first physical line of a multi-line command).
 */
export function foldedKeyArgument(
  name: string,
  call: ToolCallPayload,
  result: ToolResultPayload | null = null,
): FoldedKeyArgument | null {
  const record = asRecord(knowledgeValue(call.input));
  const changedPath = result?.changes[0]?.path ?? null;
  const pathArg = (value: string | null): FoldedKeyArgument | null =>
    value ? { text: value, title: value } : null;
  switch (name) {
    case "Bash":
    case "run_terminal_command": {
      const command = asString(record?.command);
      const full = command ?? singleLinePreview(knowledgeValue(call.input));
      return full ? { text: firstLine(full) ?? full, title: full } : null;
    }
    case "Edit":
    case "Write":
    case "Read":
    case "NotebookEdit":
      return pathArg(asString(record?.file_path) ?? asString(record?.notebook_path) ?? changedPath);
    case "read_file":
      // list_dir's fields are accepted too: both render as the Read family.
      return pathArg(
        asString(record?.target_file) ??
          asString(record?.target_directory) ??
          asString(record?.file_path) ??
          changedPath,
      );
    case "list_dir":
      return pathArg(asString(record?.target_directory) ?? asString(record?.path) ?? changedPath);
    case "write":
    case "search_replace":
      return pathArg(asString(record?.file_path) ?? changedPath);
    default:
      return null;
  }
}

/**
 * A shell call's working directory from its completed frame: grok puts
 * `current_dir` in rawOutput, not the tool input (fixture tui-updates.jsonl
 * line 9). `structuredResult` holds the whole terminal update frame. Shared by
 * the presenter and ToolCard's BashCard so the lookup lives in one place.
 */
export function resultCurrentDir(result: ToolResultPayload | null): string | null {
  if (!result) return null;
  const structured = asRecord(knowledgeValue(result.structuredResult));
  return asString(asRecord(structured?.rawOutput)?.current_dir);
}

/** Human copy for a grok workflow `source` discriminator (design §3.1). */
const RHAI_SOURCE_COPY: Record<string, string> = {
  script: "Rhai 脚本",
  script_path: "脚本路径",
  resume: "恢复运行",
  pause: "暂停运行",
  stop: "停止运行",
  name: "运行名",
};

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

  // --- grok native tools (design grok-structural-translation §3.1/§6.4).
  // `name` is the stable native name; the card heading is the ACP frame's
  // human `title` (call.displayTitle), with the native name shown as the
  // card's secondary label by ToolCard. Field names are grok's own
  // (`command`, `target_file`, `file_path`, `questions[]`, …), not Claude's.
  const grokTitle = (fallback: string): string => knowledgeValue(call.displayTitle) ?? fallback;

  if (name === "run_terminal_command") {
    const command = asString(record?.command) ?? "";
    const details: ToolDetail[] = [{ label: "$", value: command, pre: true }];
    // The input has no cwd while the call runs; the completed frame carries
    // rawOutput.current_dir (fixture line 9).
    const cwd = asString(record?.current_dir) ?? asString(record?.cwd) ?? resultCurrentDir(result);
    if (cwd) details.push({ label: "目录", value: cwd });
    if (record?.is_background === true) details.push({ label: "后台", value: "是" });
    const exit = result ? knowledgeValue(result.exitCode) : undefined;
    if (exit !== undefined && exit !== null) details.push({ label: "exit", value: String(exit) });
    return {
      title: grokTitle("Shell"),
      subtitle: asString(record?.description) ?? (command.split("\n")[0] || null),
      status: state,
      details: withResult(details, result, "输出"),
    };
  }

  if (name === "read_file") {
    const path =
      asString(record?.target_file) ?? asString(record?.file_path) ?? result?.changes[0]?.path ?? "file";
    const details: ToolDetail[] = [{ label: "路径", value: path }];
    const offset = record?.offset;
    const limit = record?.limit;
    if (typeof offset === "number" || typeof limit === "number") {
      details.push({ label: "范围", value: `第 ${String(offset ?? 1)} 行起${limit ? `，${String(limit)} 行` : ""}` });
    }
    return { title: grokTitle("Read"), subtitle: path, status: state, details: withResult(details, result, "内容") };
  }

  if (name === "list_dir") {
    const path = asString(record?.target_directory) ?? asString(record?.path) ?? "dir";
    return {
      title: grokTitle("List Dir"),
      subtitle: path,
      status: state,
      details: withResult([{ label: "目录", value: path }], result, "内容"),
    };
  }

  if (name === "write") {
    const path = asString(record?.file_path) ?? result?.changes[0]?.path ?? "file";
    const details: ToolDetail[] = [{ label: "路径", value: path }];
    const changed = result?.changes.length ?? 0;
    if (changed) details.push({ label: "改动", value: `${changed} 处` });
    return { title: grokTitle("Write"), subtitle: path, status: state, details };
  }

  if (name === "search_replace") {
    const path = asString(record?.file_path) ?? result?.changes[0]?.path ?? "file";
    const details: ToolDetail[] = [{ label: "路径", value: path }];
    if (record?.old_string != null || record?.new_string != null) {
      details.push({ label: "替换", value: "old_string → new_string" });
    }
    const changed = result?.changes.length ?? 0;
    if (changed) details.push({ label: "改动", value: `${changed} 处` });
    return { title: grokTitle("Edit"), subtitle: path, status: state, details: withResult(details, result) };
  }

  if (name === "spawn_subagent") {
    const description = asString(record?.description) ?? asString(record?.prompt) ?? null;
    const details: ToolDetail[] = [];
    const type = asString(record?.subagent_type);
    if (type) details.push({ label: "子代理", value: type });
    const isolation = asString(record?.isolation);
    if (isolation) details.push({ label: "隔离", value: isolation });
    const prompt = asString(record?.prompt);
    if (prompt) details.push({ label: "任务", value: prompt, pre: true, fold: prompt.length > 200 });
    return { title: grokTitle("Task"), subtitle: description, status: state, details: withResult(details, result) };
  }

  if (name === "workflow") {
    // The Rhai run discriminator lives in `source` (name / script /
    // script_path / resume / pause / stop). Parse the Rhai meta with the Rhai
    // opener; without a meta literal the card shows the source kind and a
    // truncated script — never a blank heading.
    const sourceRec = asRecord(record?.source);
    const sourceString = typeof record?.source === "string" ? (record.source as string) : null;
    const script = asString(sourceRec?.script) ?? asString(record?.script) ?? "";
    const scriptPath = asString(sourceRec?.script_path);
    const sourceKind = sourceRec
      ? (["resume", "pause", "stop", "name", "script_path", "script"] as const).find(
          (key) => sourceRec[key] !== undefined && sourceRec[key] !== null,
        ) ?? null
      : sourceString
        ? "name"
        : null;
    const meta = parseWorkflowMeta(script, { opener: RHAI_META_OPENER });
    const kindLine = sourceKind ? RHAI_SOURCE_COPY[sourceKind] ?? sourceKind : "Rhai 工作流";
    const subtitle =
      meta.description ??
      meta.name ??
      asString(record?.description) ??
      (sourceKind === "script_path" && scriptPath ? scriptPath : kindLine);
    const details: ToolDetail[] = [];
    if (meta.name) details.push({ label: "工作流", value: meta.name });
    if (sourceKind) details.push({ label: "来源", value: kindLine });
    // The run identifier: source.name strings the run, and resume/pause/stop
    // carry the id the control call addresses. Show it so the card names the
    // run it acts on.
    const runId = sourceString
      ? sourceString
      : sourceRec
        ? asString(sourceRec.name) ??
          asString(sourceRec.resume) ??
          asString(sourceRec.pause) ??
          asString(sourceRec.stop)
        : null;
    if (runId) details.push({ label: "运行", value: runId });
    if (state === "running") details.push({ label: "状态", value: "在后台运行 · /workflows 可查看进度" });
    if (script) details.push({ label: "脚本", value: truncate(script), pre: true, fold: true });
    else if (scriptPath) details.push({ label: "脚本路径", value: scriptPath });
    return { title: grokTitle("Workflow"), subtitle, status: state, details: withResult(details, result) };
  }

  if (name === "ask_user_question") {
    // Usually projected to an interaction and rendered as QuestionForm; this
    // card is the fallback for a tool-only frame.
    const questions = Array.isArray(record?.questions) ? record.questions : [];
    const first = asRecord(questions[0]);
    const questionText = asString(first?.question);
    const options = Array.isArray(first?.options) ? first.options : [];
    const labels = options
      .map((option) => asString(asRecord(option)?.label) ?? null)
      .filter((value): value is string => value !== null);
    const multi = first?.multiSelect === true;
    // The question text is the card subtitle; body rows carry only what the
    // subtitle does not (the option labels, and the answer once it lands).
    const details: ToolDetail[] = [];
    if (labels.length) details.push({ label: multi ? "选项（多选）" : "选项", value: labels.join(" / ") });
    return {
      title: grokTitle("Question"),
      subtitle: questionText,
      status: state,
      details: withResult(details, result, "回答"),
    };
  }

  // MCP: a Claude `mcp__server__tool` qualified name, or a grok call to the
  // explicit `use_tool` / `search_tool` MCP tools. A bare `__` in any other
  // name is not enough — the registry's tightened heuristic owns that call.
  if (name.startsWith("mcp__") || name === "use_tool" || name === "search_tool") {
    let server: string;
    let tool: string;
    if (name.startsWith("mcp__")) {
      ({ server, tool } = splitMcpName(name));
    } else {
      const qualified = asString(record?.tool_name) ?? name;
      ({ server, tool } = splitGrokMcpName(qualified));
    }
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
