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
import { compactInput, humanDuration, parseWorkflowMeta, presentTool } from "./toolPresenters";

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
