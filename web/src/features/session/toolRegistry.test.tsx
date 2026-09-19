import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { known, type Id } from "../../types/wire";
import {
  familyFor,
  isInteractionTool,
  registryKey,
  shouldFoldToolCard,
  TOOL_FAMILIES,
} from "./toolRegistry";
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

/** A final (settled) result for a Claude-family card. */
function doneResult(outcome: ToolResultPayload["outcome"] = "succeeded"): ToolResultPayload {
  return {
    nodeId: "obj_n" as Id,
    revision: "2",
    operation: "replace",
    baseRevision: "1",
    toolCallId: "obj_c" as Id,
    stage: "final",
    outcome,
    blocks: [],
    structuredResult: known({}),
    exitCode: known(0),
    changes: [],
  };
}

/**
 * Stub the workbench media query. `compact=true` makes the COMPACT_WORKBENCH_QUERY
 * match (a 390px layout); anything else reads as the desktop default.
 */
function stubLayout(compact: boolean) {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: compact && query.includes("max-width: 767px"),
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }));
}

afterEach(() => vi.unstubAllGlobals());

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
    expect(screen.getByText("/repo/src/main.rs")).toBeTruthy();
    // limit is a line count, not an end line: "from line 10, 40 lines".
    expect(screen.getByText("第 10 行起，40 行")).toBeTruthy();
    expect(container.textContent).not.toContain("10-40");
    expect(screen.queryByText("file")).toBeNull();
    expect(screen.queryByText("target_file")).toBeNull();
    expect(container.textContent).not.toContain("offset");
  });

  it("prints the native name exactly once when displayTitle equals the tool name", () => {
    // On main the adapter sets display_title = tool_name; the human ACP title
    // arrives only with the D-043 statusless-update translation. The heading
    // then is the native name and the secondary label must not duplicate it.
    const { container } = renderGrok(
      grokCall("run_terminal_command", "run_terminal_command", { command: "printf x" }),
    );
    expect(screen.queryByTestId("tool-native-name")).toBeNull();
    expect((container.textContent ?? "").match(/run_terminal_command/g)).toHaveLength(1);
  });

  it("does not duplicate the native name in the folded row either", () => {
    // Running cards are D-041-exempt and never fold, so this is a settled card
    // folded by an explicit collapse-all request on a non-compact layout.
    const { container } = render(
      <ToolCard
        driverKind="shell-pty"
        call={grokCall("read_file", "read_file", { target_file: "/a.rs" })}
        result={grokResult({})}
        completeness="structured"
        diffState="unknown"
        defaultFolded
      />,
    );
    expect(screen.queryByTestId("tool-native-name")).toBeNull();
    expect((container.textContent ?? "").match(/read_file/g)).toHaveLength(1);
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

/**
 * D-041 fold decision table (ui-spec.md §2.2): family × settled × compact,
 * with every exemption.
 */
describe("shouldFoldToolCard · D-041 decision table", () => {
  it("folds an ordinary settled card on compact, never on desktop by default", () => {
    expect(shouldFoldToolCard({ family: "Bash", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "Edit", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "Read", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "Write", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "Task", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "MCP", settled: true, compact: true })).toBe(true);
    expect(shouldFoldToolCard({ family: "Generic", settled: true, compact: true })).toBe(true);
    // Desktop default state is unchanged.
    expect(shouldFoldToolCard({ family: "Bash", settled: true, compact: false })).toBe(false);
  });

  it("never folds a running / unsettled card under the automatic compact fold", () => {
    expect(shouldFoldToolCard({ family: "Bash", settled: false, compact: true })).toBe(false);
    expect(shouldFoldToolCard({ family: "Bash", settled: false, compact: false })).toBe(false);
    // collapse-all still folds it — see the requested test below.
  });

  it("never folds the Workflow family under the automatic compact fold", () => {
    expect(
      shouldFoldToolCard({ family: "Workflow", settled: true, compact: true }),
    ).toBe(false);
    expect(
      shouldFoldToolCard({ family: "Workflow", settled: false, compact: true }),
    ).toBe(false);
    // collapse-all still folds it — see the requested test below.
  });

  it("never folds error (failed/denied) cards", () => {
    expect(
      shouldFoldToolCard({ family: "Bash", settled: true, compact: true, failed: true }),
    ).toBe(false);
    expect(
      shouldFoldToolCard({
        family: "Bash",
        settled: true,
        compact: false,
        failed: true,
        requested: true,
      }),
    ).toBe(false);
  });

  it("never folds interaction.* cards", () => {
    expect(
      shouldFoldToolCard({ family: "Generic", settled: true, compact: true, interaction: true }),
    ).toBe(false);
    expect(isInteractionTool("AskUserQuestion")).toBe(true);
    expect(isInteractionTool("ask_user_question")).toBe(true);
    expect(isInteractionTool("Bash")).toBe(false);
    expect(isInteractionTool(undefined)).toBe(false);
  });

  it("collapse-all (requested) folds every non-failed card, including running and Workflow", () => {
    // Ruling: 全部折叠 keeps main's exact behaviour — D-041 exemptions govern
    // only the automatic compact fold.
    expect(
      shouldFoldToolCard({ family: "Bash", settled: true, compact: false, requested: true }),
    ).toBe(true);
    expect(
      shouldFoldToolCard({ family: "Generic", settled: true, compact: false, requested: true }),
    ).toBe(true);
    // A still-running card folds under collapse-all.
    expect(
      shouldFoldToolCard({ family: "Bash", settled: false, compact: false, requested: true }),
    ).toBe(true);
    // The Workflow family folds under collapse-all too.
    expect(
      shouldFoldToolCard({ family: "Workflow", settled: true, compact: false, requested: true }),
    ).toBe(true);
    // Failed/denied cards are the single exception, exactly as main.
    expect(
      shouldFoldToolCard({
        family: "Bash",
        settled: true,
        compact: false,
        failed: true,
        requested: true,
      }),
    ).toBe(false);
  });
});

/**
 * Rendered D-041 rows: the compact one-liner for finished ordinary cards,
 * what it carries, and the exemptions that keep full cards mounted.
 */
describe("rendered D-041 compact folds", () => {
  it("folds a settled Bash card to family + command first line; expanding mounts the full card", () => {
    stubLayout(true);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "echo workflow-running" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    let card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("1");
    const arg = screen.getByTestId("tool-fold-arg");
    expect(arg.textContent).toBe("echo workflow-running");
    expect(arg.getAttribute("title")).toBe("echo workflow-running");
    // The family word is the heading — no redundant duplicate family chip.
    expect(card.textContent).toContain("Bash");
    expect((card.textContent ?? "").match(/Bash/g)).toHaveLength(1);
    // The full card body is not mounted while folded.
    expect(screen.queryByText("$ echo workflow-running")).toBeNull();
    // The toggle exposes the fold state and is distinguishable across rows.
    const toggle = screen.getByTestId("tool-fold-open");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.getAttribute("aria-label")).toBe("展开 Bash echo workflow-running");

    fireEvent.click(toggle);
    card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("0");
    expect(screen.getByText("$ echo workflow-running")).toBeTruthy();
    expect(screen.getByText(/exit 0/)).toBeTruthy();
  });

  it("shows only the first command line but exposes the full command in title", () => {
    stubLayout(true);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "set -e\ncargo test" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    const arg = screen.getByTestId("tool-fold-arg");
    expect(arg.textContent).toBe("set -e");
    expect(arg.getAttribute("title")).toBe("set -e\ncargo test");
    expect(screen.getByTestId("tool-card").textContent).not.toContain("cargo test");
  });

  it("folds a settled Edit card to family + the path", () => {
    stubLayout(true);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Edit", { file_path: "/repo/src/main.rs", old_string: "a", new_string: "b" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
    const arg = screen.getByTestId("tool-fold-arg");
    expect(arg.textContent).toBe("/repo/src/main.rs");
    expect(arg.getAttribute("title")).toBe("/repo/src/main.rs");
    expect(screen.queryByText("拟修改")).toBeNull();
  });

  it("keeps the desktop default unfolded with the same settled card", () => {
    stubLayout(false);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "ninja -C build" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.getByText("$ ninja -C build")).toBeTruthy();
  });

  it("an expanded card stays expanded when its virtualised row unmounts and remounts", () => {
    // The transcript owns the expansion (Set keyed by node id): the row
    // unmounts ~8 rows outside the virtual window, so a card-local latch
    // would silently re-fold on the way back.
    stubLayout(true);
    const onExpand = vi.fn();
    const cardProps = {
      driverKind: "claude-print",
      call: call("Bash", { command: "echo survives-remount" }),
      result: doneResult(),
      completeness: "structured",
      diffState: "unknown" as const,
    };
    const { unmount } = render(<ToolCard {...cardProps} expanded={false} onExpand={onExpand} />);
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");

    fireEvent.click(screen.getByTestId("tool-fold-open"));
    expect(onExpand).toHaveBeenCalledTimes(1);

    // The transcript row scrolls away: React unmounts the card entirely.
    unmount();
    expect(screen.queryByTestId("tool-card")).toBeNull();

    // The row scrolls back; the transcript re-renders with the stored state.
    render(<ToolCard {...cardProps} expanded onExpand={onExpand} />);
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    // The full card body is mounted without another click.
    expect(screen.getByText("$ echo survives-remount")).toBeTruthy();
    expect(screen.queryByTestId("tool-fold-open")).toBeNull();
  });

  it("collapse-all folds even a running card on desktop (main behaviour)", () => {
    stubLayout(false);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "still going" })}
        result={null}
        completeness="structured"
        diffState="unknown"
        defaultFolded
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
  });

  it("collapse-all folds the Workflow family on desktop (main behaviour)", () => {
    stubLayout(false);
    const k = <T,>(value: T) => known(value);
    render(
      <MemoryRouter initialEntries={["/s/ins_wf"]}>
        <ToolCard
          driverKind="claude-print"
          call={call("Workflow", { script: "export const meta = {}" })}
          result={null}
          completeness="structured"
          diffState="unknown"
          defaultFolded
          workflow={{
            run: {
              workflowId: "wf_unit",
              engine: "claude-workflow",
              nativeRunId: k("wf_native"),
              nativeTaskId: k("task"),
              toolCallId: "obj_c",
              state: "running",
              revision: "1",
              title: k("demo"),
              name: k("demo-wf"),
              description: k("demo workflow"),
              totals: null,
              live: null,
              note: null,
              launchedAt: null,
              resultRef: null,
            },
            phases: [],
            members: [],
          }}
        />
      </MemoryRouter>,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
    expect(screen.queryByTestId("workflow-card")).toBeNull();
  });

  it("a running card seen live folds the moment its final result settles (no reload)", () => {
    stubLayout(true);
    const { rerender } = render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "long task" })}
        result={null}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.queryByTestId("tool-fold-arg")).toBeNull();

    // D-041 ruling: the result lands live (no remount, no reload) — the card
    // folds immediately; a live phone session is the scroll problem the fold
    // exists for. The reader can expand it again.
    rerender(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "long task" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    const card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("1");
    expect(screen.getByTestId("tool-fold-arg").textContent).toBe("long task");
    expect(screen.queryByText("$ long task")).toBeNull();
  });

  it("folds a card that mounts already settled (settled history) on compact", () => {
    stubLayout(true);
    // A fresh mount — e.g. scrolled history, or a turn completed before the
    // reader opened it — starts folded even though the call is done.
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "long task" })}
        result={doneResult()}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
    expect(screen.getByTestId("tool-fold-arg").textContent).toBe("long task");
  });

  it("keeps a failed (error) card open on compact", () => {
    stubLayout(true);
    render(
      <ToolCard
        driverKind="claude-print"
        call={call("Bash", { command: "make test" })}
        result={doneResult("failed")}
        completeness="structured"
        diffState="unknown"
      />,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.queryByTestId("tool-fold-open")).toBeNull();
  });

  it("keeps an interaction fallback card (ask_user_question) open on compact", () => {
    stubLayout(true);
    renderGrok(
      grokCall("ask_user_question", "Ask: Choose one.", {
        questions: [{ question: "Choose one.", options: [], multiSelect: null }],
      }),
      grokResult({}),
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.queryByTestId("tool-fold-open")).toBeNull();
  });

  it("never folds the Workflow family: the live timeline card stays mounted", () => {
    stubLayout(true);
    const k = <T,>(value: T) => known(value);
    render(
      <MemoryRouter initialEntries={["/s/ins_wf"]}>
        <ToolCard
          driverKind="claude-print"
          call={call("Workflow", { script: "export const meta = {}" })}
          result={null}
          completeness="structured"
          diffState="unknown"
          workflow={{
            run: {
              workflowId: "wf_unit",
              engine: "claude-workflow",
              nativeRunId: k("wf_native"),
              nativeTaskId: k("task"),
              toolCallId: "obj_c",
              state: "running",
              revision: "1",
              title: k("demo"),
              name: k("demo-wf"),
              description: k("demo workflow"),
              totals: null,
              live: null,
              note: null,
              launchedAt: null,
              resultRef: null,
            },
            phases: [
              {
                workflowId: "wf_unit",
                phaseId: "p1",
                nativePhaseId: k("p1"),
                label: k("Review"),
                state: "running",
                revision: "1",
                parentPhaseId: null,
              },
            ],
            members: [],
          }}
        />
      </MemoryRouter>,
    );
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    expect(screen.getByTestId("workflow-card").getAttribute("data-status")).toBe("running");
  });
});
