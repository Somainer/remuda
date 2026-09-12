import { useState, type ReactNode } from "react";
import type { Observation } from "../../types/observation";
import { knowledgeValue } from "../../types/command";
import { MarkdownText } from "../../components/MarkdownText";
import { formatTokens } from "../../lib/format";
import ui from "../../styles/ui.module.css";
import { assembleTranscript, compactTranscript } from "./assemble";
import { ToolCard } from "./ToolCard";

export function Transcript({ events, compact = true }: { events: Observation[]; compact?: boolean }) {
  const nodes = compactTranscript(assembleTranscript(events), compact);
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12, padding: "12px 16px 24px" }}>
      {nodes.map((node) => {
        if (node.type === "message") {
          return (
            <section key={node.id}>
              <div className={ui.you}>{node.role === "user" ? "You" : node.role}</div>
              {node.role === "assistant" ? <MarkdownText text={node.text} /> : <p style={{ margin: 0 }}>{node.text}</p>}
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
              driverKind="claude-print"
              call={node.call}
              result={node.result}
              completeness={node.completeness}
              diffState={node.diffState}
            />
          );
        }
        if (node.type === "workflow") {
          return (
            <article key={node.id} className={ui.card}>
              <div className={ui.cardHead}>
                <strong>Workflow</strong>
                <span>{knowledgeValue(node.run.title) ?? node.run.state}</span>
              </div>
              <ul>
                {node.members.map((m) => (
                  <li key={m.memberId}>
                    {knowledgeValue(m.label) ?? m.memberId} · {m.state}
                    {knowledgeValue(m.modelResolved) ? ` · ${knowledgeValue(m.modelResolved)}` : ""}
                  </li>
                ))}
              </ul>
            </article>
          );
        }
        if (node.type === "usage") {
          const input = formatTokens(node.payload.inputTokens);
          const output = formatTokens(node.payload.outputTokens);
          if (!input || !output) return null;
          const cost = node.payload.cost.state === "known" ? ` · $${node.payload.cost.value.amount}` : "";
          return (
            <div key={node.id} className={ui.usage}>
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
                    driverKind="claude-print"
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
        if (node.type === "opaque") {
          return (
            <div key={node.id} className={ui.listMeta}>
              未识别事件 · {node.kind}
            </div>
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
      <button className={ui.chip} onClick={() => setOpen(!open)}>
        {open ? "收起过程" : `${toolCount} 次工具 · ${thoughtCount} 段思考`}
      </button>
      {open ? <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>{children}</div> : null}
    </div>
  );
}
