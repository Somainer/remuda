/**
 * Drill-in view for one subagent transcript.
 *
 * Route: `/s/:instanceId/agents/:agentId` (c-wfdrill). The subagent is a
 * Claude sub-session inside the session, not a Remuda instance, so this is a
 * bounded on-demand read (never a live subscription): the Node parses
 * `agent-<id>.jsonl` through the normal transcript pipeline and this view
 * runs the same {@link assembleTranscript} projection the structured tab
 * uses. Back navigation restores the parent's saved reading position.
 */
import { useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { MarkdownText } from "../../../components/MarkdownText";
import {
  assembleTranscript,
  compactTranscript,
  isToolFailure,
  type TranscriptNode,
} from "../assemble";
import { ToolCard } from "../ToolCard";
import sessionCss from "../session.module.css";
import { fmtDuration, fmtTokens } from "../workflow/workflowProgress";
import {
  fetchSubagentTranscript,
  type SubagentMeta,
  type SubagentTranscriptResponse,
} from "./subagentApi";
import css from "./subagent.module.css";

type LoadState =
  | { status: "loading" }
  | { status: "starting"; reason?: string }
  | { status: "error"; message: string }
  | { status: "ready"; data: SubagentTranscriptResponse };

/** What the Node's `reason` means for a human reading the row. */
function startingNote(reason?: string): string {
  if (reason === "transcript-unbound") {
    return "宿主尚未绑定这个会话的 transcript，因此还读不到它的子会话。";
  }
  return "子会话已创建，它自己的 transcript 尚未落盘。";
}

function elapsedMs(meta: SubagentMeta): number | undefined {
  if (!meta.startedAt || !meta.endedAt) return undefined;
  const start = Date.parse(meta.startedAt);
  const end = Date.parse(meta.endedAt);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return undefined;
  return Math.max(0, end - start);
}

function NodeRow({ node }: { node: TranscriptNode }) {
  if (node.type === "message") {
    if (node.role === "user") {
      return (
        <section className={sessionCss.user} data-testid="subagent-message" data-role="user">
          <div className={sessionCss.you}>{node.origin === "human" ? "human" : "prompt"}</div>
          <p className={sessionCss.bubble}>{node.text}</p>
        </section>
      );
    }
    return (
      <section className={sessionCss.assistant} data-testid="subagent-message" data-role="assistant">
        <div className={sessionCss.you}>assistant</div>
        <MarkdownText text={node.text} />
      </section>
    );
  }
  if (node.type === "thought") {
    return (
      <details className={sessionCss.thought}>
        <summary>▸ thinking</summary>
        <p>{node.text}</p>
      </details>
    );
  }
  if (node.type === "tool") {
    const card = (
      <ToolCard
        driverKind={node.driverKind}
        call={node.call}
        result={node.result}
        completeness={node.completeness}
        diffState={node.diffState}
      />
    );
    if (!isToolFailure(node)) return card;
    return (
      <div className={css.failWrap} data-tool-outcome={node.result?.outcome}>
        {card}
      </div>
    );
  }
  if (node.type === "usage") {
    return null;
  }
  return null;
}

export function SubagentView() {
  const { instanceId = "", agentId = "" } = useParams();
  const [state, setState] = useState<LoadState>({ status: "loading" });

  useEffect(() => {
    let cancelled = false;
    setState({ status: "loading" });
    void fetchSubagentTranscript(instanceId, agentId)
      .then((data) => {
        if (cancelled) return;
        setState(
          data.available ? { status: "ready", data } : { status: "starting", reason: data.reason },
        );
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        const message = error instanceof Error ? error.message : String(error);
        setState({ status: "error", message });
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId, agentId]);

  const nodes = useMemo<TranscriptNode[]>(() => {
    if (state.status !== "ready") return [];
    // Subagent transcripts are sidechain records; no optimistic bubbles.
    return compactTranscript(assembleTranscript(state.data.events), true);
  }, [state]);

  const decodedAgent = (() => {
    try {
      return decodeURIComponent(agentId);
    } catch {
      return agentId;
    }
  })();

  return (
    <div className={css.page} data-testid="subagent-view">
      <div className={css.backBar}>
        <Link className={css.backLink} to={`/s/${instanceId}`} data-testid="subagent-back">
          ← 返回会话
        </Link>
        <span className={css.title} title={decodedAgent}>
          子会话 · {decodedAgent.length > 16 ? `${decodedAgent.slice(0, 14)}…` : decodedAgent}
        </span>
      </div>
      {state.status === "loading" ? <div className={css.notice}>正在读取子会话…</div> : null}
      {state.status === "starting" ? (
        <div className={css.notice} data-testid="subagent-starting">
          启动中：{startingNote(state.reason)}
        </div>
      ) : null}
      {state.status === "error" ? (
        <div className={css.notice} data-testid="subagent-error">
          无法读取子会话：{state.message}
        </div>
      ) : null}
      {state.status === "ready" ? (
        <>
          <SubagentHeader meta={state.data.meta} />
          <div className={css.scroller} data-testid="subagent-scroller">
            <div className={css.list}>
              {nodes.map((node) => (
                <NodeRow key={node.id} node={node} />
              ))}
            </div>
          </div>
        </>
      ) : null}
    </div>
  );
}

function SubagentHeader({ meta }: { meta?: SubagentMeta }) {
  if (!meta) return null;
  const elapsed = elapsedMs(meta);
  return (
    <div className={css.backBar} data-testid="subagent-header">
      {meta.model ? <span className={css.meta}>model · {meta.model}</span> : null}
      {typeof meta.calls === "number" ? <span className={css.meta}>{meta.calls} tool calls</span> : null}
      {typeof meta.tokens === "number" && meta.tokens > 0 ? (
        <span className={css.meta}>{fmtTokens(meta.tokens)} tokens</span>
      ) : null}
      {elapsed ? <span className={css.meta}>{fmtDuration(elapsed)}</span> : null}
      {meta.runId ? <span className={css.meta}>{meta.runId}</span> : null}
      {meta.prompt ? <div className={css.prompt}>{meta.prompt}</div> : null}
    </div>
  );
}
