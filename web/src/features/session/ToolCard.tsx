import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString, jsonPreview } from "../../lib/format";
import { DiffBlock } from "../../components/DiffBlock";
import ui from "../../styles/ui.module.css";
import { familyFor, splitMcpName } from "./toolRegistry";
import type { DiffState } from "./assemble";

function asTextBlocks(result: ToolResultPayload | null): string {
  if (!result) return "";
  return result.blocks
    .map((b) => (b.type === "text" ? b.text : ""))
    .filter(Boolean)
    .join("\n");
}

function BashCard({ call, result, completeness }: { call: ToolCallPayload; result: ToolResultPayload | null; completeness: string }) {
  const input = knowledgeValue(call.input);
  const rec = asRecord(input);
  const command = asString(rec?.command) ?? jsonPreview(input);
  const exit = result ? knowledgeValue(result.exitCode) : undefined;
  const running = !result || result.stage !== "final";
  return (
    <article className={`${ui.card} ${completeness === "partial" ? ui.cardPartial : ""}`}>
      <div className={ui.cardHead}>
        <strong>Bash</strong>
        <span className={ui.pill}>{running ? "running" : exit === undefined ? "无 exit" : `exit ${exit}`}</span>
        {completeness !== "structured" ? <span>不完整</span> : null}
      </div>
      <pre className={ui.pre}>{`$ ${command}`}</pre>
      {result ? (
        <details>
          <summary>stdout</summary>
          <pre className={ui.pre}>{asTextBlocks(result) || "ninja: no work to do."}</pre>
        </details>
      ) : null}
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
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>{family}</strong>
        <span className={ui.path}>{path}</span>
      </div>
      <DiffBlock path={path} diff={diff} state={diffState} />
    </article>
  );
}

function ReadCard({ call }: { call: ToolCallPayload }) {
  const rec = asRecord(knowledgeValue(call.input));
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>Read</strong>
        <span className={ui.path}>{asString(rec?.file_path) ?? "file"}</span>
      </div>
    </article>
  );
}

function WorkflowCard({
  call,
  runTitle,
  members,
}: {
  call: ToolCallPayload;
  runTitle?: string;
  members?: { label: string; state: string }[];
}) {
  const rec = asRecord(knowledgeValue(call.input));
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>Workflow</strong>
        <span>{runTitle ?? "running"}</span>
      </div>
      {rec?.script ? <pre className={ui.pre}>{asString(rec.script)}</pre> : null}
      <ul>
        {(members ?? []).map((m) => (
          <li key={m.label}>
            {m.label} · {m.state}
          </li>
        ))}
      </ul>
    </article>
  );
}

function TaskCard({ call }: { call: ToolCallPayload }) {
  const rec = asRecord(knowledgeValue(call.input));
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>Task</strong>
      </div>
      <pre className={ui.pre}>{asString(rec?.prompt) ?? jsonPreview(rec)}</pre>
    </article>
  );
}

function McpCard({ call, result }: { call: ToolCallPayload; result: ToolResultPayload | null }) {
  const name = knowledgeValue(call.toolName) ?? "mcp";
  const { server, tool } = splitMcpName(name);
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>MCP</strong>
        <span>
          {server}/{tool}
        </span>
      </div>
      <details>
        <summary>参数</summary>
        <pre className={ui.pre}>{jsonPreview(knowledgeValue(call.input))}</pre>
      </details>
      {result ? (
        <details>
          <summary>结果</summary>
          <pre className={ui.pre}>{asTextBlocks(result) || jsonPreview(knowledgeValue(result.structuredResult))}</pre>
        </details>
      ) : null}
    </article>
  );
}

function GenericCard({ call, result }: { call: ToolCallPayload; result: ToolResultPayload | null }) {
  const name = knowledgeValue(call.displayTitle) ?? knowledgeValue(call.toolName) ?? "tool";
  return (
    <article className={ui.card}>
      <div className={ui.cardHead}>
        <strong>{name}</strong>
        <span>Generic</span>
      </div>
      <details open>
        <summary>入参</summary>
        <pre className={ui.pre}>{jsonPreview(knowledgeValue(call.input))}</pre>
      </details>
      {result ? (
        <details>
          <summary>出参</summary>
          <pre className={ui.pre}>{jsonPreview(result)}</pre>
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
}: {
  driverKind: string;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: string;
  diffState: DiffState;
  workflowTitle?: string;
  workflowMembers?: { label: string; state: string }[];
}) {
  const name = knowledgeValue(call.toolName);
  const family = familyFor(driverKind, name);
  if (family === "Bash") return <BashCard call={call} result={result} completeness={completeness} />;
  if (family === "Edit" || family === "Write") return <EditWriteCard family={family} call={call} result={result} diffState={diffState} />;
  if (family === "Read") return <ReadCard call={call} />;
  if (family === "Workflow") return <WorkflowCard call={call} runTitle={workflowTitle} members={workflowMembers} />;
  if (family === "Task") return <TaskCard call={call} />;
  if (family === "MCP") return <McpCard call={call} result={result} />;
  return <GenericCard call={call} result={result} />;
}
