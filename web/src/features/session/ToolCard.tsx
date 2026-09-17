import { useState } from "react";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString, jsonPreview } from "../../lib/format";
import { DiffBlock } from "../../components/DiffBlock";
import { familyFor, splitMcpName } from "./toolRegistry";
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

function BashCard({ call, result, completeness }: { call: ToolCallPayload; result: ToolResultPayload | null; completeness: string }) {
  const input = knowledgeValue(call.input);
  const rec = asRecord(input);
  const command = asString(rec?.command) ?? jsonPreview(input);
  const exit = result ? knowledgeValue(result.exitCode) : undefined;
  const running = !result || result.stage !== "final";
  const stdout = asTextBlocks(result);
  const lines = stdout ? stdout.split("\n").length : 0;
  return (
    <article className={`${css.tool} ${completeness === "partial" ? css.toolPartial : ""}`}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>Bash</span>
        <span className={css.toolStatus}>
          {running ? <span className={css.runDot} /> : null}
          {running ? <LiveToolElapsed call={call} /> : exit === undefined ? "无 exit" : `exit ${exit}`}
        </span>
        {completeness !== "structured" ? <span className={css.stat}>不完整</span> : null}
        <span className={css.spacer} />
        <span className={css.stat}>{cwdOf(call) ?? call.toolCallId}</span>
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
}: {
  family: "Edit" | "Write";
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  diffState: DiffState;
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
        <span className={css.toolTitle}>{family}</span>
        <span className={css.path}>{path}</span>
        {stat ? <span className={css.stat}>{stat}</span> : null}
        <span className={css.spacer} />
        <span className={badge}>{label}</span>
      </div>
      <DiffBlock path={path} diff={diff} state={diffState} />
    </article>
  );
}

function ReadCard({ call, result }: { call: ToolCallPayload; result: ToolResultPayload | null }) {
  const rec = asRecord(knowledgeValue(call.input));
  const offset = rec?.offset;
  const limit = rec?.limit;
  const range = typeof offset === "number" || typeof limit === "number" ? `:${String(offset ?? 1)}-${String(limit ?? "")}` : "";
  const snippet = result
    ? result.blocks
        .map((b) => (b.type === "text" ? b.text : ""))
        .filter(Boolean)
        .join("\n")
    : "";
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>Read</span>
        <span className={css.path}>
          {asString(rec?.file_path) ?? "file"}
          {range}
        </span>
      </div>
      {snippet ? <pre className={css.stdout}>{snippet}</pre> : null}
    </article>
  );
}

function McpCard({ call, result }: { call: ToolCallPayload; result: ToolResultPayload | null }) {
  const name = knowledgeValue(call.toolName) ?? "mcp";
  const { server, tool } = splitMcpName(name);
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>MCP</span>
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
  call,
  result,
  completeness,
}: {
  name: string;
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
}) {
  const [folded, setFolded] = useState(defaultFolded);
  const shown = settle ? result : null;
  const name = knowledgeValue(call.displayTitle) ?? knowledgeValue(call.toolName) ?? "tool";
  const family = familyFor(driverKind, knowledgeValue(call.toolName));
  if (folded) {
    return (
      <article className={css.tool} data-testid="tool-card" data-folded="1">
        <div className={css.toolHead}>
          <span className={css.toolTitle}>{name}</span>
          <span className={css.stat}>{family}</span>
          <span className={css.spacer} />
          <button type="button" className={css.openBtn} onClick={() => setFolded(false)}>
            展开
          </button>
        </div>
      </article>
    );
  }
  const inner =
    family === "Bash" ? (
      <BashCard call={call} result={shown} completeness={completeness} />
    ) : family === "Edit" || family === "Write" ? (
      <EditWriteCard family={family} call={call} result={shown} diffState={diffState} />
    ) : family === "Read" ? (
      <ReadCard call={call} result={shown} />
    ) : family === "Workflow" && workflow ? (
      // r-ux-w: the timeline card hangs directly on this tool row, visible by
      // default; the presenter card is the fallback when no run data exists.
      <WorkflowTimelineCard
        run={workflow.run}
        phases={workflow.phases}
        members={workflow.members}
        subagents={workflow.subagents}
      />
    ) : family === "MCP" ? (
      <McpCard call={call} result={shown} />
    ) : (
      <PresentedCard name={name} call={call} result={shown} completeness={completeness} />
    );
  return (
    <div data-testid="tool-card" data-folded="0">
      {inner}
    </div>
  );
}
