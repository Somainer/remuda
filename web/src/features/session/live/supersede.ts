/**
 * Supersede the hook-streamed bubble with the transcript message — in the
 * projection, never on the wire (live-view design §2.3, §2.5).
 *
 * The journal keeps both records: the hook `MessageDisplay` node (a line-
 * level display echo in the hook's own id space) and the transcript record
 * (`msg_vrtx_*`, written ~50 ms around it) can never share an id. When the
 * transcript text has the streamed text as a prefix within the same stream,
 * the transcript is authoritative: the streamed node stops rendering and the
 * transcript node takes its slot, so the row the reader is watching upgrades
 * in place — one bubble, no reflow, no second card. A streamed node no
 * transcript confirms is left untouched and keeps its `streaming` chrome.
 */
import type { Observation, ToolResultPayload } from "../../../types/generated";
import { knowledgeValue } from "../../../types/command";
import type { TranscriptNode } from "../assemble";
import { toolFingerprint } from "./phase";

type MessageNode = Extract<TranscriptNode, { type: "message" }>;
type ToolNode = Extract<TranscriptNode, { type: "tool" }>;

function isAssistantMessage(node: TranscriptNode): node is MessageNode {
  return node.type === "message" && node.role === "assistant" && !node.local;
}

/** Whitespace-normalised text, so wrapping/indentation differences cannot
 *  defeat a prefix that genuinely holds. */
function norm(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

/**
 * Collapse streamed/transcript message pairs. Pure: returns the same array
 * reference when nothing is superseded so callers keep memo identity.
 *
 * The events are needed because after the hook fold closes, both the
 * streamed node and the transcript node carry `status: "complete"` and the
 * same origin — `source.channel` (`hook` vs `transcript`) is the only honest
 * discriminator, and it lives on the observations, not on the assembled
 * node.
 */
export function supersedeStreamed(
  nodes: readonly TranscriptNode[],
  events: readonly Observation[],
): TranscriptNode[] {
  // node id → producing channel, for messages and tool calls alike.
  const channelById = new Map<string, string>();
  for (const ev of events) {
    if (ev.kind === "message") {
      channelById.set(ev.payload.nodeId, ev.source.channel);
      channelById.set(ev.payload.messageId, ev.source.channel);
    } else if (ev.kind === "tool_call" || ev.kind === "tool_result") {
      channelById.set(ev.payload.toolCallId, ev.source.channel);
    }
  }

  const streamed: number[] = [];
  const authoritative: number[] = [];
  nodes.forEach((node, index) => {
    if (!isAssistantMessage(node)) return;
    const channel = channelById.get(node.id);
    if (channel === "hook") streamed.push(index);
    else if (channel === "transcript") authoritative.push(index);
  });

  const replacement = new Map<number, MessageNode>(); // streamed slot → winner
  const removed = new Set<number>(); // authoritative slots removed from
  const usedAuthoritative = new Set<number>();

  if (streamed.length > 0 && authoritative.length > 0) {
    for (const sIndex of streamed) {
      const streamedNode = nodes[sIndex] as MessageNode;
      const prefix = norm(streamedNode.text);
      if (!prefix) continue;
      // Pair with the nearest unused transcript node whose text confirms the
      // stream. Nearest-by-position stands in for the promptId grouping the
      // wire does not carry onto message observations: the strict-prefix test
      // is the real collision guard.
      let best = -1;
      let bestDistance = Number.POSITIVE_INFINITY;
      for (const aIndex of authoritative) {
        if (usedAuthoritative.has(aIndex)) continue;
        const candidate = nodes[aIndex] as MessageNode;
        const text = norm(candidate.text);
        if (!text.startsWith(prefix)) continue;
        const distance = Math.abs(aIndex - sIndex);
        if (distance < bestDistance) {
          best = aIndex;
          bestDistance = distance;
        }
      }
      if (best < 0) continue;
      usedAuthoritative.add(best);
      removed.add(best);
      replacement.set(sIndex, nodes[best] as MessageNode);
    }
  }

  // Tool convergence runs unconditionally — the hook/transcript tool pair
  // exists even in a turn with no streamed text.
  if (replacement.size === 0) return convergeTools(nodes as TranscriptNode[], channelById);

  const out: TranscriptNode[] = [];
  nodes.forEach((node, index) => {
    if (removed.has(index) && !replacement.has(index)) return;
    out.push(replacement.get(index) ?? node);
  });
  return convergeTools(out, channelById);
}

/**
 * Merge Final results across tiers for the same tool. The transcript record
 * is authoritative for output text, but on the promoted path it carries no
 * exit code while the hook's `tool_response` does — one card needs both.
 */
function mergeResults(base: ToolResultPayload | null, extra: ToolResultPayload | null): ToolResultPayload | null {
  if (!base) return extra;
  if (!extra) return base;
  const knownElse = <T,>(ours: { state?: string; value?: T } | undefined, theirs: { state?: string; value?: T } | undefined) =>
    ours && ours.state === "known" ? ours : theirs ?? ours;
  return {
    ...base,
    // Final wins over partial; stages are Final on both tiers here.
    stage: base.stage === "final" || extra.stage === "final" ? "final" : base.stage,
    exitCode: knownElse(base.exitCode, extra.exitCode) as ToolResultPayload["exitCode"],
    structuredResult: knownElse(base.structuredResult, extra.structuredResult) as ToolResultPayload["structuredResult"],
    outcome: base.outcome ?? extra.outcome,
    blocks: base.blocks.length ? base.blocks : extra.blocks,
    changes: base.changes.length ? base.changes : extra.changes,
  };
}

/**
 * Converge the hook running-tool node and the transcript's node for one tool
 * call onto a single card (design §2.3, projection side).
 *
 * The two tiers do not yet share a derived node id on every code path, but
 * the hook payload and the transcript block carry identical `tool_name` +
 * `tool_input`, which is exactly what the elapsed anchor is fingerprinted by.
 * Before Final the hook running card survives (it owns the live anchor and
 * the ticker); after Final the transcript identity is authoritative and the
 * hook's exit code/text are merged onto it. The converged card keeps the
 * earliest slot — the hook card's — so the row upgrades in place.
 */
function convergeTools(
  nodes: readonly TranscriptNode[],
  channelById: ReadonlyMap<string, string>,
): TranscriptNode[] {
  const groups = new Map<string, number[]>();
  nodes.forEach((node, index) => {
    if (node.type !== "tool") return;
    const fp = toolFingerprint(node.name, knowledgeValue(node.call.input));
    const list = groups.get(fp) ?? [];
    list.push(index);
    groups.set(fp, list);
  });

  // slot index → converged node, for an in-place upgrade
  const winnerAtSlot = new Map<number, ToolNode>();
  const drop = new Set<number>();
  for (const indices of groups.values()) {
    if (indices.length < 2) continue;
    const slot = indices[0];
    const members = indices.map((i) => nodes[i] as ToolNode);
    const anyFinal = members.some((node) => node.result?.stage === "final");
    // Before Final the hook running card is the live row. Once finished the
    // transcript node is the authoritative identity; its missing exit code
    // is filled from the hook result below.
    const winnerIndex = anyFinal
      ? (indices.find((i) => channelById.get((nodes[i] as ToolNode).call.toolCallId) === "transcript") ??
        indices.find((i) => (nodes[i] as ToolNode).result?.stage === "final") ??
        slot)
      : indices.find((i) => channelById.get((nodes[i] as ToolNode).call.toolCallId) === "hook") ?? slot;
    const winner = nodes[winnerIndex] as ToolNode;
    // Combine every tier's result on the surviving node.
    const result = members.reduce<ToolResultPayload | null>(
      (merged, node) => mergeResults(merged, node.result),
      winner.result,
    );
    winnerAtSlot.set(slot, { ...winner, result });
    for (const i of indices) {
      // Every non-slot position goes away; the slot itself is replaced by the
      // converged node below.
      if (i !== slot) drop.add(i);
    }
  }
  if (drop.size === 0) return nodes as TranscriptNode[];

  const out: TranscriptNode[] = [];
  nodes.forEach((node, index) => {
    const winner = winnerAtSlot.get(index);
    if (winner) {
      out.push(winner);
      return;
    }
    if (drop.has(index)) return;
    out.push(node);
  });
  return out;
}
