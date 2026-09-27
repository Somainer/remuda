/**
 * Folded live tool rows of one subagent, rendered UNDER its parent row.
 *
 * The main transcript keeps only main-agent turns plus the parent rows; here
 * the reader expands a subagent's in-flight tool calls without leaving the
 * session. The full transcript (prompt, model, tokens, final text) lives in
 * the drill-in view the header links to.
 */
import { createContext, useContext, useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import type { SubagentRef, ToolNode } from "../assemble";
import { ToolCard } from "../ToolCard";
import { isToolFailure } from "../assemble";
import sessionCss from "../toolCard.module.css";
import css from "./subagent.module.css";

/** Route for a subagent's full drill-in transcript. */
export function subagentHref(instanceId: string, agentId: string): string {
  return `/s/${encodeURIComponent(instanceId)}/agents/${encodeURIComponent(agentId)}`;
}

/**
 * Expansion of rows nested under a parent tool (Task subagent folds and
 * workflow member folds). The transcript virtualises rows, so a fold's own
 * useState would be lost when the parent scrolls out of the overscan, and
 * 全部折叠 could not reach it. The transcript provides its session-scoped
 * `expandedTools` set here (keyed by child node id, and by `nestedFoldKey`
 * for the fold itself), plus the child holding the current search hit.
 * Without a provider (drill-in views, unit tests) everything stays local.
 */
export type NestedToolState = {
  openChildId: string | null;
  expanded: ReadonlySet<string>;
  onToggle: (id: string, expanded: boolean) => void;
};

export const NestedToolContext = createContext<NestedToolState | null>(null);

/** Set key for the open state of the fold listing one agent's tool rows. */
export function nestedFoldKey(kind: "subagent" | "member", agentId: string): string {
  return `${kind}-fold:${agentId}`;
}

/**
 * Open state of one nested fold: the reader's toggle (shared set when
 * provided), forced open while it holds the current search hit.
 */
export function useNestedFold(
  key: string,
  nodes: readonly ToolNode[],
  openChildId?: string | null,
): { open: boolean; toggle: () => void; hitId: string | null; state: NestedToolState | null } {
  const state = useContext(NestedToolContext);
  const [local, setLocal] = useState(false);
  const hitId = openChildId ?? state?.openChildId ?? null;
  const manual = state ? state.expanded.has(key) : local;
  const forced = Boolean(hitId && nodes.some((node) => node.id === hitId));
  const toggle = () => {
    if (state) state.onToggle(key, !manual);
    else setLocal((value) => !value);
  };
  return { open: manual || forced, toggle, hitId, state };
}

/**
 * Controlled expansion for one nested card: the searched row opens past its
 * settled fold; the others follow the shared set (or the card's own state
 * without a provider).
 */
export function nestedCardExpansion(
  node: ToolNode,
  hitId: string | null,
  state: NestedToolState | null,
): { expanded?: boolean; onExpand?: () => void } {
  if (!state) return { expanded: hitId === node.id ? true : undefined };
  return {
    expanded: hitId === node.id || state.expanded.has(node.id),
    onExpand: () => state.onToggle(node.id, true),
  };
}

function lastToolName(ref: SubagentRef): string | null {
  for (let i = ref.nodes.length - 1; i >= 0; i -= 1) {
    const name = ref.nodes[i].name;
    if (name) return name;
  }
  return null;
}

function DrillInLink({ agentId, children }: { agentId: string; children: ReactNode }) {
  const { instanceId = "" } = useParams();
  return (
    <Link
      className={sessionCss.openBtn}
      data-testid="subagent-open"
      to={subagentHref(instanceId, agentId)}
    >
      {children}
    </Link>
  );
}

/**
 * One subagent fold under a plain Task row.
 *
 * Live-only summary: hook observations carry no token usage, so the strip
 * shows tool count and the last tool; tokens/elapsed live in the drill-in
 * view (parsed from the agent transcript).
 */
export function SubagentFold({
  subagent,
  openChildId,
}: {
  subagent: SubagentRef;
  openChildId?: string | null;
}) {
  const { open, toggle, hitId, state } = useNestedFold(
    nestedFoldKey("subagent", subagent.agentId),
    subagent.nodes,
    openChildId,
  );
  const calls = subagent.nodes.length;
  const last = lastToolName(subagent);
  return (
    <div className={css.fold} data-testid="subagent-fold" data-agent-id={subagent.agentId}>
      <div className={css.head}>
        <button
          type="button"
          className={css.toggle}
          aria-expanded={open}
          data-testid="subagent-fold-toggle"
          onClick={toggle}
        >
          <span className={css.caret}>{open ? "▾" : "▸"}</span>
          <span>
            {calls} tool calls{last ? ` · ${last}` : ""}
          </span>
        </button>
        <DrillInLink agentId={subagent.agentId}>打开子会话</DrillInLink>
      </div>
      {open ? (
        <div className={css.rows}>
          {subagent.nodes.map((node) =>
            node.result && isToolFailure(node) ? (
              <div key={node.id} className={css.failWrap} data-tool-outcome={node.result.outcome}>
                <ToolCard
                  driverKind={node.driverKind}
                  call={node.call}
                  result={node.result}
                  completeness={node.completeness}
                  diffState={node.diffState}
                />
              </div>
            ) : (
              <ToolCard
                key={node.id}
                driverKind={node.driverKind}
                call={node.call}
                result={node.result}
                completeness={node.completeness}
                diffState={node.diffState}
                foldSettled
                {...nestedCardExpansion(node, hitId, state)}
              />
            ),
          )}
        </div>
      ) : null}
    </div>
  );
}

/** All Task-spawned subagent folds on one parent tool row. */
export function SubagentFolds({ refs, openChildId }: { refs: SubagentRef[]; openChildId?: string | null }) {
  if (!refs.length) return null;
  return (
    <div className={css.folds}>
      {refs.map((subagent) => (
        <SubagentFold key={subagent.agentId} subagent={subagent} openChildId={openChildId} />
      ))}
    </div>
  );
}
