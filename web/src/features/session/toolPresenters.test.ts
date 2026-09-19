/**
 * Per-tool presenters (D-028 P3 feedback A).
 *
 * The user compared the 结构 view with Claude Code's own TUI: a `Workflow`
 * call rendered as an empty card, and `TaskOutput` dumped raw JSON. These
 * check the parsing that turns each call into something a human can read.
 *
 * Inputs are the shapes recorded from a real claude 2.1.221 session (see
 * `crates/remuda-driver/tests/fixtures/claude-transcript-workflow.jsonl`).
 */
import { describe, expect, it } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { known, type Id } from "../../types/wire";
import {
  compactInput,
  humanDuration,
  parseWorkflowMeta,
  presentTool,
  resultMedia,
  resultText,
  RHAI_META_OPENER,
} from "./toolPresenters";

function call(name: string, input: unknown): ToolCallPayload {
  return {
    nodeId: "n" as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: "toolu_1" as Id,
    parentToolCallId: null,
    toolName: known(name),
    displayTitle: known(name),
    category: "other",
    input: known(input),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
  };
}

function result(text: string, outcome: ToolResultPayload["outcome"] = "succeeded"): ToolResultPayload {
  return {
    nodeId: "n" as Id,
    revision: "2",
    operation: "replace",
    baseRevision: "1",
    toolCallId: "toolu_1" as Id,
    stage: "final",
    outcome,
    blocks: [{ type: "text", text }],
    structuredResult: known({}),
    exitCode: known(0),
    changes: [],
  };
}

/** The exact script shape a recorded Workflow call carried. */
const SCRIPT = `export const meta = { name: 'p3-demo', description: 'P3 transcript shape probe', phases: [{ title: 'Only' }] }
phase('Only')
const r = await agent('Reply with the single word PONG and nothing else.')
return { r }`;

describe("parseWorkflowMeta", () => {
  it("reads the name and description out of the meta literal", () => {
    expect(parseWorkflowMeta(SCRIPT)).toEqual({
      name: "p3-demo",
      description: "P3 transcript shape probe",
    });
  });

  it("does not stop at the brace inside phases", () => {
    // `phases: [{ title: … }]` closes a brace before the literal ends; a naive
    // scan would truncate the body and lose whatever follows.
    const script = `export const meta = {
      phases: [{ title: 'A' }, { title: 'B' }],
      name: 'after-phases',
      description: 'found anyway',
    }`;
    expect(parseWorkflowMeta(script)).toEqual({
      name: "after-phases",
      description: "found anyway",
    });
  });

  it.each([
    ['export const meta = { name: "dq", description: "double" }', "dq", "double"],
    ["export const meta = { name: `bt`, description: `backtick` }", "bt", "backtick"],
  ])("accepts %s quoting", (script, name, description) => {
    expect(parseWorkflowMeta(script)).toEqual({ name, description });
  });

  it("returns nothing rather than throwing on a script it cannot parse", () => {
    // A blank card is recoverable via the `description` input; an exception
    // would take the whole transcript row down.
    expect(parseWorkflowMeta("phase('Only')\nawait agent('hi')")).toEqual({});
    expect(parseWorkflowMeta("")).toEqual({});
  });
});

describe("presentTool · Workflow", () => {
  it("titles the card from the script's meta, not 'running'", () => {
    const view = presentTool("Workflow", call("Workflow", { script: SCRIPT }), null);
    expect(view.title).toBe("Workflow");
    expect(view.subtitle).toBe("P3 transcript shape probe");
    expect(view.status).toBe("running");
    expect(view.details).toContainEqual({ label: "工作流", value: "p3-demo" });
    expect(view.details.some((d) => d.value.includes("/workflows"))).toBe(true);
  });

  it("falls back to the description input when the script has no meta", () => {
    const view = presentTool(
      "Workflow",
      call("Workflow", { script: "phase('x')", description: "from the input" }),
      null,
    );
    expect(view.subtitle).toBe("from the input");
  });

  it("shows the result summary once the workflow reports back", () => {
    const launched = "Workflow launched in background. Task ID: wc5tri90t\nSummary: P3 transcript shape probe";
    const view = presentTool("Workflow", call("Workflow", { script: SCRIPT }), result(launched));
    expect(view.status).toBe("done");
    expect(view.details.some((d) => d.value.includes("wc5tri90t"))).toBe(true);
  });
});

describe("presentTool · TaskOutput", () => {
  it("says what it is waiting for, in words, instead of dumping JSON", () => {
    const view = presentTool(
      "TaskOutput",
      call("TaskOutput", { task_id: "w7dujkwfx", block: true, timeout: 600000 }),
      null,
    );
    expect(view.title).toBe("Task Output");
    expect(view.subtitle).toBe("等待任务 w7dujkwfx，最长 10 分钟");
    expect(view.details).toContainEqual({ label: "任务", value: "w7dujkwfx" });
  });

  it("distinguishes a non-blocking status check from a wait", () => {
    const view = presentTool("TaskOutput", call("TaskOutput", { task_id: "abc", block: false }), null);
    expect(view.subtitle).toBe("查询任务 abc 的当前状态");
  });

  it("shows the output once it lands", () => {
    const view = presentTool(
      "TaskOutput",
      call("TaskOutput", { task_id: "abc", block: true, timeout: 60000 }),
      result("PONG"),
    );
    expect(view.status).toBe("done");
    expect(view.details).toContainEqual(
      expect.objectContaining({ label: "输出", value: "PONG" }),
    );
  });
});

describe("presentTool · common tools", () => {
  it("shows a subagent's description and type", () => {
    const view = presentTool(
      "Agent",
      call("Agent", { description: "Map the signal bus", subagent_type: "Explore", prompt: "look at X" }),
      null,
    );
    expect(view.title).toBe("Task");
    expect(view.subtitle).toBe("Map the signal bus");
    expect(view.details).toContainEqual({ label: "子代理", value: "Explore" });
  });

  it("shows a command and its cwd", () => {
    const view = presentTool("Bash", call("Bash", { command: "ninja -C build", cwd: "/work/repo" }), null);
    expect(view.details).toContainEqual({ label: "$", value: "ninja -C build", pre: true });
    expect(view.details).toContainEqual({ label: "目录", value: "/work/repo" });
  });

  it("shows a path and line range for a read", () => {
    const view = presentTool("Read", call("Read", { file_path: "/work/repo/a.rs", offset: 10, limit: 40 }), null);
    expect(view.subtitle).toBe("/work/repo/a.rs");
    expect(view.details).toContainEqual({ label: "范围", value: "第 10 行起，40 行" });
  });

  it("marks a failed result as failed rather than done", () => {
    const view = presentTool("Bash", call("Bash", { command: "false" }), result("boom", "failed"));
    expect(view.status).toBe("failed");
  });
});

describe("presentTool · unknown tools", () => {
  it("summarises the input as key/value rather than raw JSON", () => {
    const view = presentTool("NotARealTool", call("NotARealTool", { foo: "bar", count: 3 }), null);
    expect(view.title).toBe("NotARealTool");
    expect(view.details).toContainEqual({ label: "foo", value: "bar", pre: false });
    expect(view.details).toContainEqual({ label: "count", value: "3", pre: false });
  });

  it("handles an input that is not an object at all", () => {
    expect(compactInput("just text")).toEqual([{ label: "输入", value: "just text", pre: false }]);
    expect(compactInput(undefined)).toEqual([]);
  });
});

/** A call whose displayTitle is the ACP human title, like the adapter emits. */
function titledCall(name: string, title: string, input: unknown): ToolCallPayload {
  return { ...call(name, input), displayTitle: known(title) };
}

/** A final shell result carrying the completed frame's rawOutput. */
function grokShellResult(rawOutput: Record<string, unknown>): ToolResultPayload {
  return { ...result("exit: 0\n"), structuredResult: known({ status: "completed", rawOutput }) };
}

/**
 * Grok presenter coverage. The `run_terminal_command` and `ask_user_question`
 * inputs are verbatim from crates/remuda-driver/tests/fixtures/grok/
 * tui-updates.jsonl (line 8 normalized rawInput, lines 35/37 questions).
 */
describe("presentTool · grok run_terminal_command", () => {
  // Line 8 (statusless Running update) rawInput, verbatim from the fixture.
  const SHELL_INPUT = {
    variant: "Bash",
    command: "printf SPIKE_TOOL_OK > spike-result.txt",
    description: "Write the fixed probe marker in the throwaway directory.",
    is_background: false,
  };
  // The human title the same line carries.
  const SHELL_TITLE = "Execute `printf SPIKE_TOOL_OK > spike-result.txt`";

  it("renders the shell layout from the native command/description inputs", () => {
    const view = presentTool("run_terminal_command", titledCall("run_terminal_command", SHELL_TITLE, SHELL_INPUT), null);
    expect(view.title).toBe(SHELL_TITLE);
    expect(view.subtitle).toBe("Write the fixed probe marker in the throwaway directory.");
    expect(view.status).toBe("running");
    expect(view.details).toContainEqual({
      label: "$",
      value: "printf SPIKE_TOOL_OK > spike-result.txt",
      pre: true,
    });
  });

  it("shows the exit code and output once the completed frame lands", () => {
    const view = presentTool(
      "run_terminal_command",
      titledCall("run_terminal_command", SHELL_TITLE, SHELL_INPUT),
      result("exit: 0\n"),
    );
    expect(view.status).toBe("done");
    expect(view.details).toContainEqual({ label: "exit", value: "0" });
    expect(view.details).toContainEqual(
      expect.objectContaining({ label: "输出", value: "exit: 0\n" }),
    );
  });

  it("reads the working directory from rawOutput.current_dir on the completed frame (line 9)", () => {
    const view = presentTool(
      "run_terminal_command",
      titledCall("run_terminal_command", SHELL_TITLE, SHELL_INPUT),
      grokShellResult({ type: "Bash", exit_code: 0, current_dir: "/workspace/grok-spike" }),
    );
    expect(view.details).toContainEqual({ label: "目录", value: "/workspace/grok-spike" });
  });

  it("marks a background command", () => {
    const view = presentTool(
      "run_terminal_command",
      titledCall("run_terminal_command", SHELL_TITLE, { ...SHELL_INPUT, is_background: true }),
      null,
    );
    expect(view.details).toContainEqual({ label: "后台", value: "是" });
  });
});

describe("presentTool · grok ask_user_question", () => {
  // Line 37 normalized rawInput, verbatim from the fixture.
  const QUESTION_INPUT = {
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
  };
  // The human title from line 37.
  const QUESTION_TITLE = "Ask: Choose the probe result.";

  it("renders the question and its option labels instead of a JSON table", () => {
    const view = presentTool("ask_user_question", titledCall("ask_user_question", QUESTION_TITLE, QUESTION_INPUT), null);
    expect(view.title).toBe(QUESTION_TITLE);
    expect(view.subtitle).toBe("Choose the probe result.");
    expect(view.status).toBe("running");
    expect(view.details).toContainEqual({ label: "选项", value: "Alpha / Beta" });
    // The question text is the subtitle; it is not repeated as a body row.
    expect(view.details.some((d) => d.label === "问题")).toBe(false);
  });

  it("shows the committed answer from the completed frame", () => {
    const answered = result(
      'User has answered your questions: "Choose the probe result."="Alpha". You can now continue with the user\'s answers in mind.',
    );
    const view = presentTool(
      "ask_user_question",
      titledCall("ask_user_question", QUESTION_TITLE, QUESTION_INPUT),
      answered,
    );
    expect(view.status).toBe("done");
    expect(view.details.some((d) => d.label === "回答" && d.value.includes("Alpha"))).toBe(true);
  });

  it("labels a multiSelect question as multi-choice", () => {
    // Synthesized from docs, not captured [U]: the 1.0.30 fixture records only
    // a single-select question (line 37).
    const multi = {
      questions: [{ question: "Pick several.", options: [{ label: "A" }, { label: "B" }], multiSelect: true }],
    };
    const view = presentTool("ask_user_question", call("ask_user_question", multi), null);
    expect(view.details).toContainEqual({ label: "选项（多选）", value: "A / B" });
  });
});

describe("presentTool · other grok native inputs", () => {
  it("reads read_file.target_file with its offset/limit range", () => {
    // Synthesized from docs, not captured [U]: no read_file frame exists in
    // the 1.0.30 fixture (--no-subagents --no-plan probe).
    const view = presentTool(
      "read_file",
      call("read_file", { target_file: "/repo/src/main.rs", offset: 10, limit: 40 }),
      null,
    );
    expect(view.title).toBe("read_file");
    expect(view.subtitle).toBe("/repo/src/main.rs");
    expect(view.details).toContainEqual({ label: "范围", value: "第 10 行起，40 行" });
  });

  it("reads list_dir.target_directory", () => {
    // Synthesized from docs, not captured [U].
    const view = presentTool(
      "list_dir",
      call("list_dir", { target_directory: "/repo/src" }),
      null,
    );
    expect(view.subtitle).toBe("/repo/src");
    expect(view.details).toContainEqual({ label: "目录", value: "/repo/src" });
  });

  it("renders a spawn_subagent task with its type and isolation", () => {
    // Synthesized from docs, not captured [U].
    const view = presentTool(
      "spawn_subagent",
      titledCall("spawn_subagent", "Explore: Map the signal bus", {
        prompt: "Map the signal bus",
        description: "Bus survey",
        subagent_type: "Explore",
        isolation: "worktree",
      }),
      null,
    );
    expect(view.title).toBe("Explore: Map the signal bus");
    expect(view.subtitle).toBe("Bus survey");
    expect(view.details).toContainEqual({ label: "子代理", value: "Explore" });
    expect(view.details).toContainEqual({ label: "隔离", value: "worktree" });
  });

  it("splits a grok use_tool qualified name into server/tool", () => {
    // Synthesized from docs, not captured [U].
    const view = presentTool(
      "use_tool",
      call("use_tool", { tool_name: "drive__search_files", query: "spec" }),
      null,
    );
    expect(view.title).toBe("MCP");
    expect(view.subtitle).toBe("drive/search_files");
  });

  it("still treats an unknown name containing __ as Generic, not MCP", () => {
    const view = presentTool("plan__draft__v2", call("plan__draft__v2", { q: 1 }), null);
    expect(view.title).toBe("plan__draft__v2");
  });
});

describe("parseWorkflowMeta · grok Rhai opener", () => {
  it("reads name/description from the Rhai meta map", () => {
    // Synthesized from docs, not captured [U]: no workflow call exists in the
    // 1.0.30 fixture; the shape is grok-structural-translation.md §3.1/§3.4.
    const script = `let meta = #{ name: "release", description: "Ship the build" };
run("build");`;
    expect(parseWorkflowMeta(script, { opener: RHAI_META_OPENER })).toEqual({
      name: "release",
      description: "Ship the build",
    });
  });

  it("returns nothing when there is no Rhai meta literal", () => {
    expect(parseWorkflowMeta('run("build");', { opener: RHAI_META_OPENER })).toEqual({});
    expect(parseWorkflowMeta("", { opener: RHAI_META_OPENER })).toEqual({});
  });
});

describe("presentTool · grok workflow", () => {
  it("titles the card from the Rhai meta and shows the script", () => {
    // Synthesized from docs, not captured [U].
    const script = `let meta = #{ name: "release", description: "Ship the build" };
parallel([agent("test"), agent("docs")]);`;
    const view = presentTool(
      "workflow",
      call("workflow", { source: { script } }),
      null,
    );
    expect(view.title).toBe("workflow");
    expect(view.subtitle).toBe("Ship the build");
    expect(view.details).toContainEqual({ label: "工作流", value: "release" });
  });

  it("shows the run id a name/resume/pause/stop source addresses", () => {
    // Synthesized from docs, not captured [U].
    const byName = presentTool("workflow", call("workflow", { source: { name: "run-7" } }), null);
    expect(byName.details).toContainEqual({ label: "运行", value: "run-7" });
    const paused = presentTool("workflow", call("workflow", { source: { pause: "run-42" } }), null);
    expect(paused.details).toContainEqual({ label: "运行", value: "run-42" });
    const byString = presentTool("workflow", call("workflow", { source: "run-9" }), null);
    expect(byString.details).toContainEqual({ label: "运行", value: "run-9" });
  });

  it("falls back to the source kind plus a truncated script, never a blank card", () => {
    // Synthesized from docs, not captured [U].
    const longScript = Array.from({ length: 50 }, (_, i) => `// line ${i}`).join("\n");
    const paused = presentTool("workflow", call("workflow", { source: { pause: "run-42" } }), null);
    expect(paused.title).toBe("workflow");
    expect(paused.subtitle).toBeTruthy();
    expect(paused.subtitle).toBe("暂停运行");
    const noMeta = presentTool("workflow", call("workflow", { source: { script: longScript } }), null);
    expect(noMeta.subtitle).toBe("Rhai 脚本");
    const scriptRow = noMeta.details.find((d) => d.label === "脚本");
    expect(scriptRow).toBeDefined();
    expect(scriptRow!.value.length).toBeLessThanOrEqual(201);
  });
});

describe("humanDuration", () => {
  it.each([
    [600000, "10 分钟"],
    [60000, "1 分钟"],
    [30000, "30 秒"],
    [500, "500 毫秒"],
    [0, "未设置"],
  ])("renders %ims as %s", (ms, expected) => {
    expect(humanDuration(ms)).toBe(expected);
  });
});

describe("resultText / resultMedia (D-045 §6.2)", () => {
  function resultWith(blocks: ToolResultPayload["blocks"]): ToolResultPayload {
    return {
      nodeId: "n" as Id,
      revision: "2",
      operation: "close",
      baseRevision: "1",
      toolCallId: "toolu_1" as Id,
      stage: "final",
      outcome: "succeeded",
      blocks,
      structuredResult: known({}),
      exitCode: known(0),
      changes: [],
    };
  }

  it("exposes only text through resultText, byte-identical for text results", () => {
    const result = resultWith([
      { type: "text", text: "line one" },
      { type: "text", text: "line two" },
    ]);
    expect(resultText(result)).toBe("line one\nline two");
    expect(resultMedia(result)).toEqual([]);
    expect(resultText(null)).toBe("");
    expect(resultMedia(null)).toEqual([]);
  });

  it("exposes image blocks in order through resultMedia", () => {
    const result = resultWith([
      { type: "text", text: "seen" },
      {
        type: "image",
        objectId: "obj_shot_1" as Id,
        mediaType: "image/png",
        name: "screen-1.png",
        size: 70,
      },
      {
        type: "image",
        objectId: "obj_shot_2" as Id,
        mediaType: "image/png",
        name: null,
      },
    ]);
    expect(resultText(result)).toBe("seen");
    expect(resultMedia(result)).toEqual([
      { objectId: "obj_shot_1", mediaType: "image/png", name: "screen-1.png" },
      { objectId: "obj_shot_2", mediaType: "image/png", name: "image" },
    ]);
  });

  it("treats a degraded image (a producer-side text fallback) as text", () => {
    const result = resultWith([
      {
        type: "text",
        text: "[image not attached: image/png, 70 bytes — over the 16-byte object limit]",
      },
    ]);
    expect(resultMedia(result)).toEqual([]);
    expect(resultText(result)).toContain("image/png");
  });
});
