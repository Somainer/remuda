import type {
  ContentBlock,
  MessageOrigin,
  NodeMutation,
  Observation,
  ToolCallPayload,
  ToolResultPayload,
  UsagePayload,
  WorkflowMemberPayload,
  WorkflowPhasePayload,
  WorkflowRunPayload,
} from "../../types/generated";
import type { Interaction } from "../../types/generated";
import { knowledgeValue } from "../../types/command";
import { familyFor, type ToolFamily } from "./toolRegistry";
import type { LocalBubble } from "../../lib/store";

export type DiffState = "proposed" | "applied" | "unknown";

export type ToolNode = {
  type: "tool";
  id: string;
  family: ToolFamily;
  name: string;
  driverKind: string;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: Observation["completeness"];
  diffState: DiffState;
};

export type TranscriptNode =
  | {
      type: "message";
      id: string;
      role: "user" | "assistant" | "system";
      text: string;
      status: string;
      /**
       * Who wrote it (protocol §5.2). A Claude transcript files skill bodies,
       * slash-command markup, hook context and task notifications as `user`
       * records, so only `origin === "human"` may render as the user's own
       * bubble. Absent on pre-D-028 producers, which read as `human`: showing
       * one row too many is recoverable, hiding what someone said is not.
       */
      origin: MessageOrigin;
      local?: LocalBubble;
    }
  | { type: "thought"; id: string; text: string; completeness: Observation["completeness"] }
  | ToolNode
  | {
      type: "workflow";
      id: string;
      run: WorkflowRunPayload;
      phases: WorkflowPhasePayload[];
      members: WorkflowMemberPayload[];
    }
  | { type: "usage"; id: string; payload: UsagePayload }
  | { type: "interaction"; id: string; interaction: Interaction; pending: boolean }
  | { type: "error"; id: string; text: string }
  | { type: "opaque"; id: string; kind: string; summary: string | null; raw: unknown }
  | { type: "compact"; id: string; toolCount: number; thoughtCount: number; children: TranscriptNode[] };

export function diffState(call: ToolCallPayload, result: ToolResultPayload | null, completeness: Observation["completeness"]): DiffState {
  if (completeness === "partial" && !result) return "unknown";
  if (!result) return call.state === "proposed" ? "proposed" : "unknown";
  const app = result.changes[0]?.application;
  if (app === "applied") return "applied";
  if (app === "proposed") return "proposed";
  if (result.changes.length === 0) return "unknown";
  return "unknown";
}

function newerMutation(next: NodeMutation, current?: NodeMutation): boolean {
  if (!current) return true;
  if (BigInt(next.revision) <= BigInt(current.revision)) return false;
  // Snapshots can recover a missing prefix, but deltas need their exact base.
  return next.operation !== "append" || next.baseRevision === current.revision;
}

function messageBlocks(current: ContentBlock[], next: ContentBlock[], operation: NodeMutation["operation"], target: number | null): ContentBlock[] {
  if (operation === "close" && next.length === 0) return current;
  if (operation !== "append") return next;
  if (target === null) return current.concat(next);
  const block = current[target];
  if ((block && block.type !== "text") || next.some((delta) => delta.type !== "text")) return current;
  const blocks = current.slice();
  blocks[target] = { type: "text", text: (block?.text ?? "") + next.map((delta) => delta.type === "text" ? delta.text : "").join("") };
  return blocks;
}

function blocksText(blocks: ContentBlock[]): string {
  return blocks.flatMap((block) => block.type === "text" && block.text ? [block.text] : []).join("\n");
}

type MessageEvent = Extract<Observation, { kind: "message" }>;
type MessageNode = Extract<TranscriptNode, { type: "message" }>;

function compareMessages(a: MessageEvent, b: MessageEvent): number {
  const left = BigInt(a.payload.revision);
  const right = BigInt(b.payload.revision);
  if (left !== right) return left < right ? -1 : 1;
  // Some older producers close at the same revision. Apply their completion
  // after the content, while duplicate appends still apply only once.
  const operations = { open: 0, append: 1, replace: 2, close: 3 };
  const status = { queued: 0, unknown: 1, streaming: 2, interrupted: 3, complete: 4 };
  const rank = status[a.payload.status] - status[b.payload.status]
    || operations[a.payload.operation] - operations[b.payload.operation];
  if (rank) return rank;
  return BigInt(a.seq) < BigInt(b.seq) ? -1 : BigInt(a.seq) > BigInt(b.seq) ? 1 : 0;
}

/**
 * Rebuild each message in revision order; keep its first transcript position.
 *
 * `anchors` records each node's position key: the smallest journal seq of any
 * event that produced it. Gap backfill appends earlier-seq events to the end
 * of the store array, so encounter order cannot be used for placement — the
 * final transcript is sorted by this anchor in {@link assembleTranscript}.
 */
function assembleMessages(events: Observation[], anchors: Map<TranscriptNode, bigint>): Map<MessageEvent, MessageNode> {
  const identities = new Map<string, MessageEvent[]>();
  for (const ev of events) {
    if (ev.kind !== "message") continue;
    const group = identities.get(`message:${ev.payload.messageId}`) ?? identities.get(`node:${ev.payload.nodeId}`) ?? [];
    group.push(ev);
    identities.set(`message:${ev.payload.messageId}`, group);
    identities.set(`node:${ev.payload.nodeId}`, group);
  }
  const messages = new Map<MessageEvent, MessageNode>();
  for (const group of new Set(identities.values())) {
    let mutation: NodeMutation | undefined;
    let blocks: ContentBlock[] = [];
    let node: MessageNode | undefined;
    // Identity must be a pure function of the event *set*, never of array
    // order: gap backfill appends earlier-seq events to the end of the store's
    // array, and re-follow rebuilds it in seq order. A group[0]-derived key
    // could switch between assemblies (a regrouped message whose rename
    // arrives before its open), which would make search hits and the saved
    // reading anchor jump to another node. `nodeId` is the mutation-chain
    // identity in protocol §5.2 and survives a native `messageId` rename on
    // close, so the earliest revision's nodeId is the canonical key.
    const ordered = group.slice().sort(compareMessages);
    const stableId = ordered[0].payload.nodeId;
    for (const ev of ordered) {
      const payload = ev.payload;
      if (node && mutation?.revision === payload.revision && payload.operation === "append") {
        node.status = payload.status;
        continue;
      }
      // A retained suffix can start with any operation. Later history fills in
      // its prefix on the next assembly. Never attach a delta to a wrong base:
      // show its available suffix until the missing revisions arrive.
      const hasBase = mutation && (payload.operation !== "append" || payload.baseRevision === mutation.revision);
      blocks = messageBlocks(hasBase ? blocks : [], payload.blocks, payload.operation, payload.targetBlock);
      node ??= {
        type: "message",
        id: stableId,
        role: payload.role,
        text: "",
        status: payload.status,
        origin: "human",
      };
      node.text = blocksText(blocks);
      node.role = payload.role;
      node.status = payload.status;
      // `origin` is additive: a producer that does not classify leaves it
      // undefined, and an unclassified message must stay visible.
      node.origin = payload.origin ?? "human";
      mutation = payload;
    }
    if (node) {
      // Place the bubble at the group's earliest journal seq even when that
      // event reached the store late (gap backfill, out-of-order replay).
      const anchorSeq = group.reduce((min, ev) => {
        const v = BigInt(ev.seq);
        return v < min ? v : min;
      }, BigInt(group[0].seq));
      anchors.set(node, anchorSeq);
      let anchorEvent: MessageEvent = group[0];
      for (const ev of group) {
        if (BigInt(ev.seq) === anchorSeq) {
          anchorEvent = ev;
          break;
        }
      }
      messages.set(anchorEvent, node);
    }
  }
  return messages;
}

export function assembleTranscript(events: Observation[], bubbles: LocalBubble[] = []): TranscriptNode[] {
  // node -> earliest journal seq that produced it; the transcript is sorted
  // by this so a gap backfill (which appends late-arriving events to the
  // store array) cannot move a node to the bottom.
  const anchors = new Map<TranscriptNode, bigint>();
  const messages = assembleMessages(events, anchors);
  const thoughts = new Map<string, { mutation: NodeMutation; node: Extract<TranscriptNode, { type: "thought" }> }>();
  const tools = new Map<string, ToolNode>();
  const workflows = new Map<
    string,
    { type: "workflow"; id: string; run: WorkflowRunPayload; phases: WorkflowPhasePayload[]; members: WorkflowMemberPayload[] }
  >();
  const nodes: TranscriptNode[] = [];
  const seenUser = new Set<string>();
  const anchor = (node: TranscriptNode, ev: Observation) => {
    const seq = BigInt(ev.seq);
    const prev = anchors.get(node);
    if (prev === undefined || seq < prev) anchors.set(node, seq);
  };

  const pushTool = (node: ToolNode) => {
    tools.set(node.call.toolCallId, node);
    if (!nodes.some((n) => n.type === "tool" && n.call.toolCallId === node.call.toolCallId)) nodes.push(node);
  };

  for (const ev of events) {
    if (ev.kind === "message") {
      const node = messages.get(ev);
      if (node) nodes.push(node);
      messages.delete(ev);
      continue;
    }
    if (ev.kind === "thought") {
      const payload = ev.payload;
      const existing = thoughts.get(`thought:${payload.thoughtId}`) ?? thoughts.get(`node:${payload.nodeId}`);
      if (!newerMutation(payload, existing?.mutation)) continue;
      const node = existing?.node ?? { type: "thought" as const, id: payload.thoughtId, text: "", completeness: ev.completeness };
      if (payload.operation === "append") node.text += payload.text ?? "";
      else if (payload.operation !== "close" || payload.text !== null) node.text = payload.text ?? "";
      node.completeness = ev.completeness;
      const current = existing ?? { mutation: payload, node };
      current.mutation = payload;
      thoughts.set(`thought:${payload.thoughtId}`, current);
      thoughts.set(`node:${payload.nodeId}`, current);
      if (!existing) {
        nodes.push(node);
        anchor(node, ev);
      }
      continue;
    }
    if (ev.kind === "tool_call") {
      const call = ev.payload;
      const name = knowledgeValue(call.toolName) ?? "tool";
      const existing = tools.get(call.toolCallId);
      if (!newerMutation(call, existing?.call)) continue;
      const node: ToolNode = {
        type: "tool",
        id: call.toolCallId,
        family: familyFor(ev.source.driverKind, name),
        name,
        driverKind: ev.source.driverKind,
        call,
        result: existing?.result ?? null,
        completeness: ev.completeness,
        diffState: diffState(call, existing?.result ?? null, ev.completeness),
      };
      if (existing) {
        existing.call = call.operation === "append" ? {
          ...call,
          input: call.input.state === "known" ? call.input : existing.call.input,
          inputTextDelta: (existing.call.inputTextDelta ?? "") + (call.inputTextDelta ?? ""),
        } : call;
        existing.name = name;
        existing.family = node.family;
        existing.completeness = ev.completeness;
        existing.diffState = diffState(call, existing.result, ev.completeness);
      } else {
        pushTool(node);
        anchor(node, ev);
      }
      continue;
    }
    if (ev.kind === "tool_result") {
      const result = ev.payload;
      const existing = tools.get(result.toolCallId);
      if (existing) {
        if (!newerMutation(result, existing.result ?? undefined)) continue;
        existing.result = result;
        existing.diffState = diffState(existing.call, result, ev.completeness);
        if (ev.completeness === "partial") existing.completeness = "partial";
      }
      continue;
    }
    if (ev.kind === "workflow.run") {
      const run = ev.payload;
      const current = workflows.get(run.workflowId) ?? {
        type: "workflow" as const,
        id: run.workflowId,
        run,
        phases: [] as WorkflowPhasePayload[],
        members: [] as WorkflowMemberPayload[],
      };
      current.run = run;
      workflows.set(run.workflowId, current);
      if (!nodes.includes(current)) {
        nodes.push(current);
        anchor(current, ev);
      }
      continue;
    }
    if (ev.kind === "workflow.phase") {
      const phase = ev.payload;
      const current = workflows.get(phase.workflowId);
      if (current) current.phases = current.phases.filter((p) => p.phaseId !== phase.phaseId).concat(phase);
      continue;
    }
    if (ev.kind === "workflow.member") {
      const member = ev.payload;
      const current = workflows.get(member.workflowId);
      if (current) {
        current.members = current.members.filter((m) => m.memberId !== member.memberId).concat(member);
      }
      continue;
    }
    if (ev.kind === "usage") {
      const node = { type: "usage" as const, id: ev.eventId, payload: ev.payload };
      nodes.push(node);
      anchor(node, ev);
      continue;
    }
    if (ev.kind === "interaction.requested") {
      const interaction = ev.payload.interaction;
      const node = {
        type: "interaction" as const,
        id: ev.eventId,
        interaction,
        pending: interaction.state === "pending",
      };
      nodes.push(node);
      anchor(node, ev);
      continue;
    }
    if (ev.kind === "lifecycle") continue;
    if (ev.kind === "interaction.answered" || ev.kind === "interaction.expired") continue;
    if (ev.kind === "opaque") {
      const payload = ev.payload;
      const node = {
        type: "opaque" as const,
        id: ev.eventId,
        kind: payload.nativeType ?? ev.kind,
        summary: payload.summary ?? payload.reason ?? null,
        raw: ev.payload,
      };
      nodes.push(node);
      anchor(node, ev);
      continue;
    }
    const node = {
      type: "opaque" as const,
      id: ev.eventId,
      kind: ev.kind,
      summary: null,
      raw: ev.payload,
    };
    nodes.push(node);
    anchor(node, ev);
  }

  // Stable seq order: nodes met only through late backfill land in their
  // journal position, not at the point the store happened to see them.
  nodes
    .map((node, index) => ({ node, index, seq: anchors.get(node) }))
    .sort((a, b) => {
      if (a.seq !== undefined && b.seq !== undefined && a.seq !== b.seq) return a.seq < b.seq ? -1 : 1;
      return a.index - b.index;
    })
    .forEach((entry, i) => {
      nodes[i] = entry.node;
    });

  for (const node of nodes) {
    if (node.type === "message" && node.role === "user" && node.origin === "human") {
      seenUser.add(node.text);
    }
  }
  for (const bubble of bubbles) {
    if (bubble.state === "settled") continue;
    if (seenUser.has(bubble.text) && bubble.state !== "queued") continue;
    nodes.push({
      type: "message",
      id: bubble.id,
      role: "user",
      text: bubble.text,
      status: bubble.state,
      // A local bubble is text this user just typed into the composer.
      origin: "human",
      local: bubble,
    });
  }

  return nodes;
}

/**
 * A tool whose own result says it did not run successfully. `denied` is
 * included: a refused permission is exactly the kind of thing the reader
 * needs to see, not discover hidden behind a fold.
 */
export function isToolFailure(node: TranscriptNode): boolean {
  return node.type === "tool" && (node.result?.outcome === "failed" || node.result?.outcome === "denied");
}

/** Compact only after a turn ends (assistant or usage). In-flight tools stay expanded. */
export function compactTranscript(nodes: TranscriptNode[], enabled: boolean): TranscriptNode[] {
  if (!enabled) return nodes;
  const out: TranscriptNode[] = [];
  let pending: TranscriptNode[] = [];
  const compactPending = () => {
    // Failed tools keep their inline position in `rest`; the fold only
    // swallows successful/routine tools and thoughts.
    const tools = pending.filter((n) => n.type === "tool" && !isToolFailure(n));
    const thoughts = pending.filter((n) => n.type === "thought");
    const rest = pending.filter((n) => (n.type !== "tool" || isToolFailure(n)) && n.type !== "thought");
    if (tools.length + thoughts.length >= 2) {
      out.push({
        type: "compact",
        id: `compact:${tools[0]?.id ?? thoughts[0]?.id}`,
        toolCount: tools.length,
        thoughtCount: thoughts.length,
        children: pending.filter((n) => (n.type === "tool" && !isToolFailure(n)) || n.type === "thought"),
      });
      out.push(...rest);
    } else {
      out.push(...pending);
    }
    pending = [];
  };
  for (const node of nodes) {
    if (node.type === "message" && node.role === "user") {
      out.push(...pending);
      pending = [];
      out.push(node);
      continue;
    }
    if ((node.type === "message" && node.role === "assistant") || node.type === "usage") {
      compactPending();
      out.push(node);
      continue;
    }
    if (node.type === "interaction" && node.pending) {
      out.push(...pending);
      pending = [];
      continue;
    }
    pending.push(node);
  }
  out.push(...pending);
  return out;
}

export function collectTasks(nodes: TranscriptNode[]): ToolNode[] {
  const out: ToolNode[] = [];
  for (const node of nodes) {
    if (node.type === "tool" && node.family === "Task") out.push(node);
    if (node.type === "compact") {
      for (const child of node.children) {
        if (child.type === "tool" && child.family === "Task") out.push(child);
      }
    }
  }
  return out;
}
