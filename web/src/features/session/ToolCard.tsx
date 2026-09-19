import { useEffect, useState } from "react";
import type { ToolCallPayload, ToolResultPayload } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { asRecord, asString, jsonPreview } from "../../lib/format";
import { COMPACT_WORKBENCH_QUERY } from "../../lib/viewport";
import { DiffBlock } from "../../components/DiffBlock";
import { familyFor, isGrokTool, isInteractionTool, shouldFoldToolCard, splitGrokMcpName, splitMcpName } from "./toolRegistry";
import { foldedKeyArgument, presentTool, resultCurrentDir, resultMedia } from "./toolPresenters";
import { objectUrl } from "./AttachmentChips";
import { WorkflowTimelineCard } from "./workflow/WorkflowTimelineCard";
import { LiveToolElapsed } from "./live/LiveStatusStrip";
import type {
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../types/generated";
import type { DiffState } from "./assemble";
import css from "./session.module.css";
import foldCss from "./transcript.module.css";

function asTextBlocks(result: ToolResultPayload | null): string {
  if (!result) return "";
  return result.blocks
    .map((b) => (b.type === "text" ? b.text : ""))
    .filter(Boolean)
    .join("\n");
}

/**
 * Bounded thumbnails for image blocks a tool result staged (D-045 §6.2,
 * ui-spec §2.2): max-height lives in CSS, `loading="lazy"`, alt is the block
 * name, and a click opens the object route — never an auto-expanded lightbox.
 * An image the Node could not stage arrives as a text block, so there is no
 * broken-image branch here.
 */
function ResultMedia({ result }: { result: ToolResultPayload | null }) {
  const media = resultMedia(result);
  if (media.length === 0) return null;
  return (
    <div className={css.toolMedia} data-testid="tool-media">
      {media.map((image, index) => (
        <a
          // The Hub dedupes identical bytes to one objectId, so the index
          // keeps duplicate screenshots as distinct list items.
          key={`${image.objectId}:${index}`}
          className={css.toolMediaLink}
          href={objectUrl(image.objectId)}
          target="_blank"
          rel="noreferrer"
          data-testid="tool-media-link"
        >
          <img
            className={css.toolThumb}
            src={objectUrl(image.objectId)}
            alt={image.name}
            loading="lazy"
            data-testid="tool-thumb"
          />
        </a>
      ))}
    </div>
  );
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
 * Whether the workbench is in the compact (mobile) layout. ToolCard owns the
 * read so the D-041 default fold needs no prop plumbing through the
 * transcript; a missing matchMedia (unit DOM) reads as the desktop default.
 */
function useCompactLayout(): boolean {
  const read = () =>
    typeof window !== "undefined" && typeof window.matchMedia === "function"
      ? window.matchMedia(COMPACT_WORKBENCH_QUERY).matches
      : false;
  const [compact, setCompact] = useState(read);
  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const media = window.matchMedia(COMPACT_WORKBENCH_QUERY);
    const update = () => setCompact(media.matches);
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return compact;
}

/**
 * The stable native name as a muted secondary label (grok cards). Hidden when
 * the heading already is the native name: on main the grok adapter sets
 * display_title = tool_name, so without this guard every card printed the
 * name twice. Once the D-043 translation carries the ACP human title into
 * display_title, heading and name diverge and the label appears.
 */
function NativeLabel({ heading, name }: { heading: string; name: string }) {
  if (heading === name) return null;
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
    ? asString(rec?.current_dir) ?? asString(rec?.cwd) ?? resultCurrentDir(result) ?? cwdOf(call)
    : cwdOf(call);
  return (
    <article className={`${css.tool} ${completeness === "partial" ? css.toolPartial : ""}`}>
      <div className={css.toolHead}>
        <span className={css.toolTitle}>{grok ? displayTitle : "Bash"}</span>
        {grok ? <NativeLabel heading={displayTitle} name={nativeName} /> : null}
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
        {grok ? <NativeLabel heading={displayTitle} name={nativeName} /> : null}
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
  // `limit` is a line count, not an end line, so the copy matches the
  // presenter: "from line N, M lines" — never a misleading `10-40`.
  const range =
    typeof offset === "number" || typeof limit === "number"
      ? `第 ${String(offset ?? 1)} 行起${typeof limit === "number" && limit ? `，${String(limit)} 行` : ""}`
      : "";
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
        {grok ? <NativeLabel heading={displayTitle} name={nativeName} /> : null}
        <span className={css.path}>{path}</span>
        {range ? <span className={css.stat}>{range}</span> : null}
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
        {grok ? <NativeLabel heading="MCP" name={name} /> : null}
        <span className={css.path}>
          {server}/{tool}
        </span>
      </div>
      <details>
        <summary className={css.stdoutHead}>参数</summary>
        <pre className={css.stdout}>{jsonPreview(knowledgeValue(call.input))}</pre>
      </details>
      <ResultMedia result={result} />
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
        {grok ? <NativeLabel heading={view.title} name={name} /> : null}
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

/**
 * The D-041 compact one-line row: family + key argument, truncated to width
 * with the full value in `title`. Expanding mounts the exact same card the
 * desktop layout renders — this row changes only the default open/close.
 */
function FoldedToolRow({
  title,
  nativeName,
  displayTitle,
  grok,
  family,
  call,
  result,
  onExpand,
}: {
  title: string;
  nativeName: string;
  displayTitle: string;
  grok: boolean;
  family: ReturnType<typeof familyFor>;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  onExpand: () => void;
}) {
  const keyArg = foldedKeyArgument(nativeName, call, result);
  // The family word is redundant when the heading already is that word
  // (Claude Bash/Edit/Read/Write); Generic carries no family chip.
  const showFamily = family !== "Generic" && (grok || nativeName !== family);
  // Distinguishable name when N rows are folded: 展开 + heading + key arg.
  const expandLabel = `展开 ${title}${keyArg ? ` ${keyArg.title}` : ""}`;
  return (
    <article className={foldCss.fold} data-testid="tool-card" data-folded="1" data-family={family}>
      <div className={foldCss.foldHead}>
        <span className={foldCss.foldTitle} title={title}>
          {title}
        </span>
        {grok ? <NativeLabel heading={displayTitle} name={nativeName} /> : null}
        {showFamily ? <span className={foldCss.foldFamily}>{family}</span> : null}
        {keyArg ? (
          <span className={foldCss.foldArg} data-testid="tool-fold-arg" title={keyArg.title}>
            {keyArg.text}
          </span>
        ) : null}
        <button
          type="button"
          className={`${css.openBtn} ${foldCss.foldOpen}`}
          data-testid="tool-fold-open"
          aria-expanded={false}
          aria-label={expandLabel}
          onClick={onExpand}
        >
          展开
        </button>
      </div>
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
  expanded: expandedProp,
  onExpand: onExpandProp,
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
  /**
   * D-041: controlled expansion state owned by the transcript. Rows virtualise
   * away and remount, so a card-local latch would silently re-fold; callers
   * without a store (nested subagent rows, unit tests) leave these undefined
   * and the card falls back to local state.
   */
  expanded?: boolean;
  onExpand?: () => void;
  /** c-wfcard: persisted open/dismissed state of the mounted workflow card. */
  workflowDismissed?: boolean;
  onDismissWorkflow?: () => void;
  onUndismissWorkflow?: () => void;
}) {
  // Local fallback for callers that do not own an expansion set.
  const [localExpanded, setLocalExpanded] = useState(false);
  const userExpanded = expandedProp ?? localExpanded;
  const expand = () => {
    setLocalExpanded(true);
    onExpandProp?.();
  };
  // D-041: settled = call/result paired with a FINAL result (partial results
  // mean the call is still running); error = failed/denied outcome.
  const settled = settle && result?.stage === "final";
  const failed = settled && (result.outcome === "failed" || result.outcome === "denied");
  const compact = useCompactLayout();
  const shown = settle ? result : null;
  // Dispatch on the stable native name; the human title is the heading only.
  const nativeName = knowledgeValue(call.toolName) ?? "tool";
  const displayTitle = knowledgeValue(call.displayTitle) ?? nativeName;
  const family = familyFor(driverKind, nativeName);
  // grok's file-adapter observations are all stamped driverKind shell-pty.
  const grok = driverKind === "shell-pty" && isGrokTool(nativeName);
  // The fold decision happens AFTER family is determined. Under the automatic
  // compact fold a live card folds the instant its final result lands — a
  // live phone session is the scroll problem D-041 exists for. The
  // Workflow/error/interaction exemptions live inside shouldFoldToolCard.
  const folded =
    !userExpanded &&
    shouldFoldToolCard({
      family,
      settled,
      compact,
      failed,
      interaction: isInteractionTool(nativeName),
      requested: defaultFolded,
    });
  if (folded) {
    return (
      <FoldedToolRow
        title={grok ? displayTitle : nativeName}
        nativeName={nativeName}
        displayTitle={displayTitle}
        grok={grok}
        family={family}
        call={call}
        result={result}
        onExpand={expand}
      />
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
