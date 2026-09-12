import { useState, type ReactNode } from "react";
import type { Observation } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { MarkdownText } from "../../components/MarkdownText";
import { formatTokens, jsonPreview } from "../../lib/format";
import type { LocalBubble } from "../../lib/store";
import { hubStore } from "../../lib/store";
import ui from "../../styles/ui.module.css";
import { assembleTranscript, compactTranscript } from "./assemble";
import { ToolCard } from "./ToolCard";

export function Transcript({
  events,
  bubbles = [],
  compact = true,
}: {
  events: Observation[];
  bubbles?: LocalBubble[];
  compact?: boolean;
}) {
  const nodes = compactTranscript(assembleTranscript(events, bubbles), compact);
  return (
    <div data-testid="transcript" style={{ display: "flex", flexDirection: "column", gap: 12, padding: "12px 16px 24px" }}>
      {nodes.map((node) => {
        if (node.type === "message") {
          return (
            <section key={node.id} data-testid={node.local ? "optimistic-bubble" : "message"}>
              <div className={ui.you}>
                {node.role === "user" ? "You" : node.role}
                {node.local ? ` · ${node.local.state}` : ""}
              </div>
              {node.role === "assistant" ? <MarkdownText text={node.text} /> : <p style={{ margin: 0 }}>{node.text}</p>}
              {node.local?.state === "queued" ? (
                <button className={ui.chip} onClick={() => hubStore.retract(node.local!.id)}>
                  撤回
                </button>
              ) : null}
              {node.local?.state === "unknown" ? (
                <button className={ui.chip} onClick={() => void hubStore.send(node.local!.instanceId, node.local!.text)}>
                  仍要再送一条？
                </button>
              ) : null}
            </section>
          );
        }
        if (node.type === "thought") {
          return (
            <details key={node.id} className={ui.thought}>
              <summary>thinking{node.completeness === "screen-derived" ? " · 从屏幕猜测" : ""}</summary>
              <p>{node.text}</p>
            </details>
          );
        }
        if (node.type === "tool") {
          return (
            <ToolCard
              key={node.id}
              driverKind={node.driverKind}
              call={node.call}
              result={node.result}
              completeness={node.completeness}
              diffState={node.diffState}
            />
          );
        }
        if (node.type === "workflow") {
          const open = node.run.state === "running" || node.run.state === "failed" || node.run.state === "unknown";
          return (
            <details key={node.id} className={ui.card} open={open}>
              <summary className={ui.cardHead}>
                <strong>Workflow</strong>
                <span>{knowledgeValue(node.run.nativeRunId) ?? node.run.workflowId}</span>
                <span>{knowledgeValue(node.run.title) ?? node.run.state}</span>
              </summary>
              {node.phases.map((p) => (
                <div key={p.phaseId} style={{ marginLeft: 12 }}>
                  ▾ {knowledgeValue(p.label) ?? p.phaseId} · {p.state}
                  <ul>
                    {node.members
                      .filter((m) => !m.phaseId || m.phaseId === p.phaseId)
                      .map((m) => (
                        <li key={m.memberId}>
                          {knowledgeValue(m.label) ?? m.memberId} · {m.state}
                          {knowledgeValue(m.modelResolved) ? ` · ${knowledgeValue(m.modelResolved)}` : ""}
                        </li>
                      ))}
                  </ul>
                </div>
              ))}
              {node.phases.length === 0 ? (
                <ul>
                  {node.members.map((m) => (
                    <li key={m.memberId}>
                      {knowledgeValue(m.label) ?? m.memberId} · {m.state}
                    </li>
                  ))}
                </ul>
              ) : null}
            </details>
          );
        }
        if (node.type === "usage") {
          const input = formatTokens(node.payload.inputTokens);
          const output = formatTokens(node.payload.outputTokens);
          if (!input || !output) return null;
          const cost = node.payload.cost.state === "known" ? ` · $${node.payload.cost.value.amount}` : "";
          return (
            <div key={node.id} className={ui.usage} data-testid="usage-row">
              usage in {input} / out {output}
              {cost}
            </div>
          );
        }
        if (node.type === "compact") {
          return (
            <CompactFold key={node.id} toolCount={node.toolCount} thoughtCount={node.thoughtCount}>
              {node.children.map((child) =>
                child.type === "tool" ? (
                  <ToolCard
                    key={child.id}
                    driverKind={child.driverKind}
                    call={child.call}
                    result={child.result}
                    completeness={child.completeness}
                    diffState={child.diffState}
                  />
                ) : child.type === "thought" ? (
                  <details key={child.id} className={ui.thought}>
                    <summary>thinking</summary>
                    <p>{child.text}</p>
                  </details>
                ) : null,
              )}
            </CompactFold>
          );
        }
        if (node.type === "error") {
          return (
            <article key={node.id} className={ui.card}>
              <div className={ui.cardHead}>
                <strong>error</strong>
              </div>
              <p>{node.text}</p>
            </article>
          );
        }
        if (node.type === "opaque") {
          return (
            <details key={node.id} className={ui.listMeta}>
              <summary>未识别事件 · {node.kind}</summary>
              <pre className={ui.pre}>{jsonPreview(node.raw)}</pre>
            </details>
          );
        }
        return null;
      })}
    </div>
  );
}

function CompactFold({
  toolCount,
  thoughtCount,
  children,
}: {
  toolCount: number;
  thoughtCount: number;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button className={ui.chip} data-testid="compact-fold" onClick={() => setOpen(!open)}>
        {open ? "收起过程" : `${toolCount} 次工具 · ${thoughtCount} 段思考`}
      </button>
      {open ? <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>{children}</div> : null}
    </div>
  );
}
