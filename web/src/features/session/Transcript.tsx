import { useState, type ReactNode } from "react";
import type { Observation } from "../../types/observation";
import { MarkdownText } from "../../components/MarkdownText";
import type { LocalBubble } from "../../lib/store";
import { hubStore } from "../../lib/store";
import ui from "../../styles/ui.module.css";
import { assembleTranscript, compactTranscript } from "./assemble";
import { ToolCard } from "./ToolCard";
import { WorkflowTree } from "./WorkflowTree";
import { UsageFooter } from "./UsageFooter";
import { OpaqueRow } from "./OpaqueRow";

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
          return <WorkflowTree key={node.id} run={node.run} phases={node.phases} members={node.members} />;
        }
        if (node.type === "usage") {
          return <UsageFooter key={node.id} payload={node.payload} />;
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
          return <OpaqueRow key={node.id} kind={node.kind} summary={node.summary} raw={node.raw} />;
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
