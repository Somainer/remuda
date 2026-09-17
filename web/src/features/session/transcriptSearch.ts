import { knowledgeValue } from "../../types/command";
import { subagentToolNodes, type TranscriptNode } from "./assemble";

/**
 * In-transcript search (workbench batch E, §5 P1-2).
 *
 * The search runs over the *assembled loaded nodes*, not the DOM: the
 * transcript is virtualised, so a hit outside the rendered window would be
 * unfindable with a browser/dom-based query. Everything here is a pure
 * projection — searching must never write to the journal or drive the native
 * session; the only side effects live in the UI layer that calls these
 * functions.
 *
 * Hit identity is `(nodeId, ordinal)`. Node ids survive streaming appends
 * and regrouping (see assemble.ts), and the ordinal is the match's index
 * within that node's text. A growing tail therefore keeps the current hit;
 * a replacement that removes text makes the ordinal disappear, and the UI
 * falls back to the same node's nearest remaining match rather than jumping
 * to whatever node now sits at that list index.
 */

export type SearchMatch = {
  /** Stable node id (see `assembleTranscript`). */
  nodeId: string;
  /** Node index in the assembled list the match was computed against. */
  index: number;
  /** Match ordinal inside the node. */
  ordinal: number;
  start: number;
  end: number;
};

export type SearchOptions = {
  caseSensitive?: boolean;
};

function resultText(node: Extract<TranscriptNode, { type: "tool" }>): string {
  const parts: string[] = [node.name, node.call.toolCallId];
  const input = knowledgeValue(node.call.input);
  if (input != null) {
    try {
      parts.push(typeof input === "string" ? input : JSON.stringify(input));
    } catch {
      // Circular or otherwise un-JSON-able knowledge values still have the
      // tool name above; never let a value make the whole search throw.
    }
  }
  if (node.call.inputTextDelta) parts.push(node.call.inputTextDelta);
  const result = node.result;
  if (result) {
    if (result.outcome) parts.push(result.outcome);
    for (const block of result.blocks) {
      if (block.type === "text" && block.text) parts.push(block.text);
    }
  }
  return parts.join("\n");
}

/** All searchable text for one node; `null` for nodes with no text surface. */
export function searchableText(node: TranscriptNode): string | null {
  switch (node.type) {
    case "message":
      return node.text || null;
    case "thought":
      return node.text || null;
    case "tool":
      return resultText(node);
    case "interaction":
      return node.interaction.kind;
    case "opaque":
      return node.summary ?? node.kind;
    case "workflow": {
      const title = knowledgeValue(node.run.title);
      const labels = node.phases.map((p) => knowledgeValue(p.label) ?? "").filter(Boolean);
      return [title ?? node.id, ...labels].join("\n") || null;
    }
    case "compact": {
      // Folded process groups stay searchable: their children are loaded
      // nodes, and the UI opens the fold when a child carries the hit.
      const parts = node.children.map((child) => searchableText(child)).filter((t): t is string => Boolean(t));
      return parts.length ? parts.join("\n") : null;
    }
    case "usage":
      // Pure metric footer: nothing a reader searches for.
      return null;
  }
  return null;
}

function matchOrdinals(haystack: string, needle: string, caseSensitive: boolean): Array<[number, number]> {
  if (!needle) return [];
  const text = caseSensitive ? haystack : haystack.toLowerCase();
  const query = caseSensitive ? needle : needle.toLowerCase();
  const out: Array<[number, number]> = [];
  let at = 0;
  for (;;) {
    const found = text.indexOf(query, at);
    if (found < 0) break;
    out.push([found, found + query.length]);
    at = found + Math.max(1, query.length);
  }
  return out;
}

/** Find every match in assembled-node order. */
export function findMatches(nodes: readonly TranscriptNode[], query: string, opts: SearchOptions = {}): SearchMatch[] {
  const trimmed = query.trim();
  if (!trimmed) return [];
  const caseSensitive = opts.caseSensitive === true;
  const matches: SearchMatch[] = [];
  nodes.forEach((node, index) => {
    // Folded process groups stay searchable, but hits are attributed to the
    // child node: that is the id the UI needs to open the fold and mark the
    // actual row carrying the text.
    if (node.type === "compact") {
      for (const child of node.children) {
        emitMatches(child, index, trimmed, caseSensitive, matches);
        // A Task parent folded into the compact group can itself hold folded
        // subagent rows — keep them searchable too.
        if (child.type === "tool") {
          for (const sub of subagentToolNodes(child)) {
            emitMatches(sub, index, trimmed, caseSensitive, matches);
          }
        }
      }
      return;
    }
    // Subagent tool rows folded under Task / workflow-member parents stay
    // searchable the same way.
    if (node.type === "tool") {
      for (const child of subagentToolNodes(node)) {
        emitMatches(child, index, trimmed, caseSensitive, matches);
      }
    }
    emitMatches(node, index, trimmed, caseSensitive, matches);
  });
  return matches;
}

function emitMatches(node: TranscriptNode, index: number, query: string, caseSensitive: boolean, out: SearchMatch[]): void {
  const text = searchableText(node);
  if (!text) return;
  let ordinal = 0;
  for (const [start, end] of matchOrdinals(text, query, caseSensitive)) {
    out.push({ nodeId: node.id, index, ordinal, start, end });
    ordinal += 1;
  }
}

/**
 * Resolve the selected match after the node list changed (streaming append,
 * regroup, backfill). Prefers the same `(nodeId, ordinal)`; then the same
 * node (ordinal clamped to its last match); then the list position nearest
 * the old one. Returns 0 when there is nothing to select.
 */
export function resolveSelection(matches: readonly SearchMatch[], previous: SearchMatch | null): number {
  if (matches.length === 0) return -1;
  if (!previous) return 0;
  const exact = matches.findIndex((m) => m.nodeId === previous.nodeId && m.ordinal === previous.ordinal);
  if (exact >= 0) return exact;
  const sameNode = matches.filter((m) => m.nodeId === previous.nodeId);
  if (sameNode.length) {
    const wanted = sameNode.findIndex((m) => m.ordinal >= previous.ordinal);
    return matches.indexOf(wanted >= 0 ? sameNode[wanted] : sameNode[sameNode.length - 1]);
  }
  return Math.min(previous.index, matches.length - 1);
}
