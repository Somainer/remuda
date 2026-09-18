import { useState } from "react";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString, jsonPreview } from "../../lib/format";
import { DiffBlock } from "../../components/DiffBlock";
import { familyFor, isGrokTool, splitGrokMcpName, splitMcpName } from "./toolRegistry";
import { presentTool } from "./toolPresenters";
import { WorkflowTimelineCard } from "./workflow/WorkflowTimelineCard";
import { LiveToolElapsed } from "./live/LiveStatusStrip";
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../types/generated";
import type { DiffState } from "./assemble";
import css from "./session.module.css";

function asTextBlocks(result: ToolResultPayload | null): string {
  if (!result) return "";
  return result.blocks
    .map((b) => (b.type === "text" ? b.text : ""))
    .filter(Boolean)
    .join("\n");
}

function diffStat(diff: string): string | null {
  let add = 0;
  let del = 0;
  for (const line of diff.split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++")) add += 1;
    if (line.startsWith("-") && !line.startsWith("---")) del += 1;
  }
  if (!add && !del) return null;
  return `+${add} −${del}`;
}

function cwdOf(call: ToolCallPayload): string | null {
  const rec = asRecord(knowledgeValue(call.executor));
  return asString(rec?.workspaceId) ?? asString(rec?.cwd);
}

/**
 * A shell call's working directory from its completed frame: grok puts
 * `current_dir` in rawOutput, not the tool input (fixture line 9).
 */
function resultCwd(result: ToolResultPayload | null): string | null {
  if (!result) return null;
  const structured = asRecord(knowledgeValue(result.structuredResult));
  return asString(asRecord(structured?.rawOutput)?.current_dir);
}

/** The stable native name as a muted secondary label (grok cards). */
function NativeLabel({ name }: { name: string }) {
  return (
    <span className={css.stat} data-testid="tool-native-name">
      {name}
    </span>
  );
}

function BashCard({
  call,
  result,
  completeness,
  nativeName,
  displayTitle,
  grok,
}: {
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: string;
  nativeName: string;
  displayTitle: string;
  grok: boolean;
}) {
  const input = knowledgeValue(call.input);
  const rec = asRecord(input);
  const command = asString(rec?.command) ?? jsonPreview(input);
  const exit = result ? knowledgeValue(result.exitCode) : undefined;
  const running = !result || result.stage !== "final";
  const stdout = asTextBlocks(result);
  const lines = stdout ? stdout.split("\n").length : 0;
  // Claude's BashCard keeps its existing executor-derived cwd; grok puts the
  // working directory in the input while running and in rawOutput when done.
  const cwd = grok
    ? asString(rec?.current_dir) ?? asString(rec?.cwd) ?? resultCwd(result) ?? cwdOf(call)
    : cwdOf(call);
  return (
    <article className={`${css.tool} ${completeness === "partial" ? css.toolPartial : ""}`}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{grok ? displayTitle : "Bash"}</span>
        {grok ? <NativeLabel name={nativeName} /> : null}
        <span className={css.toolStatus}>
          {running ? <span className={css.runDot} /> : null}
          {running ? <LiveToolElapsed call={call} /> : exit === undefined ? "无 exit" : `exit ${exit}`}
        </span>
        {completeness !== "structured" ? <span className={css.stat}>不完整</span> : null}
        <span className={css.spacer} />
        <span className={css.stat}>{cwd ?? call.toolCallId}</span>
      </div>
      <pre className={css.cmd}>{`$ ${command}`}</pre>
      {result ? (
        <details>
          <summary className={css.stdoutHead}>
            ▾ stdout{lines ? ` · ${lines} 行` : ""}
          </summary>
          <pre className={css.stdout}>{stdout || "ninja: no work to do."}</pre>
        </details>
      ) : (
        <div className={css.stdoutHead}>▸ stdout</div>
      )}
    </article>
  );
}

function EditWriteCard({
  family,
  call,
  result,
  diffState,
  displayTitle,
  grok,
  nativeName,
}: {
  family: "Edit" | "Write";
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  diffState: DiffState;
  displayTitle: string;
  grok: boolean;
  nativeName: string;
}) {
  const rec = asRecord(knowledgeValue(call.input));
  const path = asString(rec?.file_path) ?? result?.changes[0]?.path ?? "file";
  const diff =
    result?.changes[0]?.diff ??
    (family === "Edit" && rec ? `@@\n-${asString(rec.old_string) ?? ""}\n+${asString(rec.new_string) ?? ""}\n` : asString(rec?.content) ?? "");
  const stat = diffStat(diff);
  const badge = diffState === "applied" ? css.applied : diffState === "unknown" ? css.unknown : css.proposed;
  const label = diffState === "applied" ? "已写入" : diffState === "unknown" ? "结果未知" : "拟修改";
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{grok ? displayTitle : family}</span>
        {grok ? <NativeLabel name={nativeName} /> : null}
        <span className={css.path}>{path}</span>
        {stat ? <span className={css.stat}>{stat}</span> : null}
        <span className={css.spacer} />
        <span className={badge}>{label}</span>
      </div>
      <DiffBlock path={path} diff={diff} state={diffState} />
    </article>
  );
}

function ReadCard({
  call,
  result,
  nativeName,
  displayTitle,
  grok,
}: {
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  nativeName: string;
  displayTitle: string;
  grok: boolean;
}) {
  const rec = asRecord(knowledgeValue(call.input));
  // Grok reads use target_file (and list_dir uses target_directory); Claude
  // uses file_path.
  const path = grok
    ? (asString(rec?.target_file) ?? asString(rec?.target_directory) ?? asString(rec?.file_path) ?? "file")
    : (asString(rec?.file_path) ?? "file");
  const offset = rec?.offset;
  const limit = rec?.limit;
  const range = typeof offset === "number" || typeof limit === "number" ? `:${String(offset ?? 1)}-${String(limit ?? "")}` : "";
  const snippet = result
    ? result.blocks
        .map((b) => (b.type === "text" ? b.text : ""))
        .filter(Boolean)
        .join("\n")
    : "";
  const heading = grok ? displayTitle : "Read";
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{heading}</span>
        {grok ? <NativeLabel name={nativeName} /> : null}
        <span className={css.path}>
          {path}
          {range}
        </span>
      </div>
      {snippet ? <pre className={css.stdout}>{snippet}</pre> : null}
    </article>
  );
}

function McpCard({
  call,
  result,
  grok,
}: {
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  grok: boolean;
}) {
  const name = knowledgeValue(call.toolName) ?? "mcp";
  // Grok reaches MCP through the explicit use_tool/search_tool calls; the
  // qualified server__tool name rides the tool_name input.
  const rec = asRecord(knowledgeValue(call.input));
  const { server, tool } = grok
    ? splitGrokMcpName(asString(rec?.tool_name) ?? name)
    : splitMcpName(name);
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>MCP</span>
        {grok ? <NativeLabel name={name} /> : null}
        <span className={css.path}>
          {server}/{tool}
        </span>
      </div>
      <details>
        <summary className={css.stdoutHead}>参数</summary>
        <pre className={css.stdout}>{jsonPreview(knowledgeValue(call.input))}</pre>
      </details>
      {result ? (
        <details>
          <summary className={css.stdoutHead}>结果</summary>
          <pre className={css.stdout}>{asTextBlocks(result) || jsonPreview(knowledgeValue(result.structuredResult))}</pre>
        </details>
      ) : null}
    </article>
  );
}

/**
 * A presenter-driven card: title, subtitle, labelled rows, and a 原始 toggle.
 *
 * This replaces the old `GenericCard`'s raw-JSON dump and the empty
 * `WorkflowCard`. The user's comparison with Claude's own TUI was that
 * `Workflow` showed nothing at all and `TaskOutput` showed raw JSON; a
 * presenter gives each tool a sentence a human can read, and keeps the raw
 * payload one click away instead of making it the default.
 */
function PresentedCard({
  name,
  grok,
  call,
  result,
  completeness,
}: {
  /** Stable native tool name — this is what presentTool dispatches on. */
  name: string;
  grok: boolean;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: string;
}) {
  const [raw, setRaw] = useState(false);
  const view = presentTool(name, call, result);
  return (
    <article className={`${css.tool} ${completeness === "partial" ? css.toolPartial : ""}`}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{view.title}</span>
        {grok && view.title !== name ? <NativeLabel name={name} /> : null}
        {view.subtitle ? <span className={css.path}>{view.subtitle}</span> : null}
        <span className={css.toolStatus} data-testid="tool-status" data-status={view.status}>
          {view.status === "running" ? <span className={css.runDot} /> : <span className={css.okDot} />}
          {view.status === "running" ? "运行中" : view.status === "failed" ? "失败" : "完成"}
        </span>
        <span className={css.spacer} />
        <button type="button" className={css.openBtn} onClick={() => setRaw(!raw)} data-testid="tool-raw-toggle">
          {raw ? "收起原始" : "原始"}
        </button>
      </div>
      {view.details.map((detail) =>
        detail.fold ? (
          <details key={detail.label}>
            <summary className={css.stdoutHead}>{detail.label}</summary>
            <pre className={css.stdout}>{detail.value}</pre>
          </details>
        ) : detail.pre ? (
          <div key={detail.label}>
            <div className={css.stdoutHead}>{detail.label}</div>
            <pre className={css.cmd}>{detail.value}</pre>
          </div>
        ) : (
          <div key={detail.label} className={css.toolHead}>
            <span className={css.stat}>{detail.label}</span>
            <span className={css.path}>{detail.value}</span>
          </div>
        ),
      )}
      {raw ? (
        <pre className={css.stdout} data-testid="tool-raw">
          {jsonPreview({ input: knowledgeValue(call.input), result })}
        </pre>
      ) : null}
    </article>
  );
}

export function ToolCard({
  driverKind,
  call,
  result,
  completeness,
  diffState,
  workflow,
  defaultFolded = false,
  settle = true,
  workflowDismissed = false,
  onDismissWorkflow,
  onUndismissWorkflow,
}: {
  driverKind: string;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: string;
  diffState: DiffState;
  /** r-ux-w: live timeline data mounted on this Workflow tool row. */
  workflow?: {
    run: WorkflowRunPayload;
    phases: WorkflowPhasePayload[];
    members: WorkflowMemberPayload[];
    /** c-wfdrill: per-member folded live tool rows. */
    subagents?: import("./assemble").SubagentRef[];
  };
  defaultFolded?: boolean;
  settle?: boolean;
  /** c-wfcard: persisted open/dismissed state of the mounted workflow card. */
  workflowDismissed?: boolean;
  onDismissWorkflow?: () => void;
  onUndismissWorkflow?: () => void;
}) {
  const [folded, setFolded] = useState(defaultFolded);
  const shown = settle ? result : null;
  // Dispatch on the stable native name; the human title is the heading only.
  const nativeName = knowledgeValue(call.toolName) ?? "tool";
  const displayTitle = knowledgeValue(call.displayTitle) ?? nativeName;
  const family = familyFor(driverKind, nativeName);
  // grok's file-adapter observations are all stamped driverKind shell-pty.
  const grok = driverKind === "shell-pty" && isGrokTool(nativeName);
  if (folded) {
    return (
      <article className={css.tool} data-testid="tool-card" data-folded="1">
        <div className={css.toolHead}>
          <span className={css.toolTitle}>{grok ? displayTitle : nativeName}</span>
          {grok ? <NativeLabel name={nativeName} /> : null}
          <span className={css.stat}>{family}</span>
          <span className={css.spacer} />
          <button type="button" className={css.openBtn} onClick={() => setFolded(false)}>
            展开
          </button>
        </div>
      </article>
    );
  }
  // grok has no workflow engine in this round (WorkflowEngine::GrokWorkflow
  // is excluded by the structural plan), so its workflow call always uses the
  // Rhai presenter card even when a run timeline happens to be mounted.
  const inner =
    family === "Bash" ? (
      <BashCard call={call} result={shown} completeness={completeness} nativeName={nativeName} displayTitle={displayTitle} grok={grok} />
    ) : family === "Edit" || family === "Write" ? (
      <EditWriteCard family={family} call={call} result={shown} diffState={diffState} displayTitle={displayTitle} grok={grok} nativeName={nativeName} />
    ) : family === "Read" ? (
      <ReadCard call={call} result={shown} nativeName={nativeName} displayTitle={displayTitle} grok={grok} />
    ) : family === "Workflow" && workflow && !grok ? (
      // r-ux-w: the timeline card hangs directly on this tool row, visible by
      // default; the presenter card is the fallback when no run data exists.
      <WorkflowTimelineCard
        run={workflow.run}
        phases={workflow.phases}
        members={workflow.members}
        subagents={workflow.subagents}
        dismissed={workflowDismissed}
        onDismiss={onDismissWorkflow}
        onUndismiss={onUndismissWorkflow}
      />
    ) : family === "MCP" ? (
      <McpCard call={call} result={shown} grok={grok} />
    ) : (
      <PresentedCard name={nativeName} call={call} result={shown} completeness={completeness} grok={grok} />
    );
  return (
    <div data-testid="tool-card" data-folded="0">
      {inner}
    </div>
  );
}
