import { useState } from "react";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString, jsonPreview } from "../../lib/format";
import { DiffBlock } from "../../components/DiffBlock";
import { familyFor, splitMcpName } from "./toolRegistry";
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
          {running ? "running · 无 exit，不画成功" : exit === undefined ? "无 exit" : `exit ${exit}`}
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

function WorkflowCard({
  runTitle,
  members,
}: {
  runTitle?: string;
  members?: { label: string; state: string }[];
}) {
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>Workflow</span>
        <span className={css.path}>{runTitle ?? "running"}</span>
      </div>
      <ul className={css.wfMembers}>
        {(members ?? []).map((m) => (
          <li key={m.label} className={css.member}>
            <span className={css.memberName}>{m.label}</span>
            <span className={css.memberMeta}>{m.state}</span>
          </li>
        ))}
      </ul>
    </article>
  );
}

function TaskCard({ call }: { call: ToolCallPayload }) {
  const rec = asRecord(knowledgeValue(call.input));
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>Task</span>
      </div>
      <pre className={css.cmd}>{asString(rec?.prompt) ?? jsonPreview(rec)}</pre>
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

function GenericCard({ call, result }: { call: ToolCallPayload; result: ToolResultPayload | null }) {
  const name = knowledgeValue(call.displayTitle) ?? knowledgeValue(call.toolName) ?? "tool";
  return (
    <article className={css.tool}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{name}</span>
        <span className={css.stat}>Generic</span>
      </div>
      <details open>
        <summary className={css.stdoutHead}>入参</summary>
        <pre className={css.stdout}>{jsonPreview(knowledgeValue(call.input))}</pre>
      </details>
      {result ? (
        <details>
          <summary className={css.stdoutHead}>出参</summary>
          <pre className={css.stdout}>{jsonPreview(result)}</pre>
        </details>
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
  workflowTitle,
  workflowMembers,
  defaultFolded = false,
  settle = true,
}: {
  driverKind: string;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: string;
  diffState: DiffState;
  workflowTitle?: string;
  workflowMembers?: { label: string; state: string }[];
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
    ) : family === "Workflow" ? (
      <WorkflowCard runTitle={workflowTitle} members={workflowMembers} />
    ) : family === "Task" ? (
      <TaskCard call={call} />
    ) : family === "MCP" ? (
      <McpCard call={call} result={shown} />
    ) : (
      <GenericCard call={call} result={shown} />
    );
  return (
    <div data-testid="tool-card" data-folded="0">
      {inner}
    </div>
  );
}
