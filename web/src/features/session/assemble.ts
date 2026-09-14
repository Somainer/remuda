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
  /**
   * r-ux-w: workflow timeline data mounted on this tool row when the
   * `workflow.run` observation named this tool call. Batch W owns
   * WorkflowTimelineCard; this hook is the only seam into the E-owned file.
   */
  workflow?: {
    run: WorkflowRunPayload;
    phases: WorkflowPhasePayload[];
    members: WorkflowMemberPayload[];
  };
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

/** Rebuild each message in revision order; keep its first transcript position. */
function assembleMessages(events: Observation[]): Map<MessageEvent, MessageNode> {
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
    for (const ev of group.slice().sort(compareMessages)) {
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
        id: group[0].payload.messageId,
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
    if (node) messages.set(group[0], node);
  }
  return messages;
}

export function assembleTranscript(events: Observation[], bubbles: LocalBubble[] = []): TranscriptNode[] {
  const messages = assembleMessages(events);
  const thoughts = new Map<string, { mutation: NodeMutation; node: Extract<TranscriptNode, { type: "thought" }> }>();
  const tools = new Map<string, ToolNode>();
  const workflows = new Map<
    string,
    { type: "workflow"; id: string; run: WorkflowRunPayload; phases: WorkflowPhasePayload[]; members: WorkflowMemberPayload[] }
  >();
  const nodes: TranscriptNode[] = [];
  const seenUser = new Set<string>();

  const pushTool = (node: ToolNode) => {
    tools.set(node.call.toolCallId, node);
    if (!nodes.some((n) => n.type === "tool" && n.call.toolCallId === node.call.toolCallId)) nodes.push(node);
  };

  // r-ux-w seam: mount a workflow's timeline card onto its Workflow tool row
  // when the run observation names the tool call. Returns true when mounted.
  const mountWorkflow = (
    current: { run: WorkflowRunPayload; phases: WorkflowPhasePayload[]; members: WorkflowMemberPayload[] },
  ): boolean => {
    const toolCallId = current.run.toolCallId;
    if (!toolCallId) return false;
    const tool = tools.get(toolCallId);
    if (!tool) return false;
    // Attach the accumulator object itself: run revisions replace `current.run`
    // and arrays mutate in place, so the mounted card stays live.
    tool.workflow = current;
    const standalone = nodes.find((n) => n.type === "workflow" && n.id === current.run.workflowId);
    if (standalone) nodes.splice(nodes.indexOf(standalone), 1);
    return true;
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
      if (!existing) nodes.push(node);
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
    if (ev.kind === "workflow.run" || ev.kind === "workflow.phase" || ev.kind === "workflow.member") {
      // Keep the per-workflow accumulator (mutated in place so a card already
      // mounted on a tool row stays live), then either mount it on the
      // Workflow tool row (decision 1) or keep a standalone node when the
      // producer gave no tool call id to attach it to.
      const wfId = ev.payload.workflowId;
      const current =
        workflows.get(wfId) ??
        (() => {
          const created = {
            type: "workflow" as const,
            id: wfId,
            run: null as unknown as WorkflowRunPayload,
            phases: [] as WorkflowPhasePayload[],
            members: [] as WorkflowMemberPayload[],
          };
          workflows.set(wfId, created);
          return created;
        })();
      if (ev.kind === "workflow.run") {
        current.run = ev.payload;
      } else if (ev.kind === "workflow.phase") {
        const phase = ev.payload;
        const at = current.phases.findIndex((p) => p.phaseId === phase.phaseId);
        if (at >= 0) current.phases[at] = phase;
        else current.phases.push(phase);
      } else {
        const member = ev.payload;
        const at = current.members.findIndex((m) => m.memberId === member.memberId);
        if (at >= 0) current.members[at] = member;
        else current.members.push(member);
      }
      if (!current.run) continue;
      const mounted = mountWorkflow(current);
      const standaloneAt = nodes.findIndex((n) => n.type === "workflow" && n.id === wfId);
      if (mounted) {
        if (standaloneAt >= 0) nodes.splice(standaloneAt, 1);
      } else if (standaloneAt < 0) {
        nodes.push(current);
      }
      continue;
    }
    if (ev.kind === "usage") {
      nodes.push({ type: "usage", id: ev.eventId, payload: ev.payload });
      continue;
    }
    if (ev.kind === "interaction.requested") {
      const interaction = ev.payload.interaction;
      nodes.push({
        type: "interaction",
        id: ev.eventId,
        interaction,
        pending: interaction.state === "pending",
      });
      continue;
    }
    if (ev.kind === "lifecycle") continue;
    if (ev.kind === "interaction.answered" || ev.kind === "interaction.expired") continue;
    if (ev.kind === "opaque") {
      const payload = ev.payload;
      nodes.push({
        type: "opaque",
        id: ev.eventId,
        kind: payload.nativeType ?? ev.kind,
        summary: payload.summary ?? payload.reason ?? null,
        raw: ev.payload,
      });
      continue;
    }
    nodes.push({
      type: "opaque",
      id: ev.eventId,
      kind: ev.kind,
      summary: null,
      raw: ev.payload,
    });
  }

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

/** Compact only after a turn ends (assistant or usage). In-flight tools stay expanded. */
export function compactTranscript(nodes: TranscriptNode[], enabled: boolean): TranscriptNode[] {
  if (!enabled) return nodes;
  const out: TranscriptNode[] = [];
  let pending: TranscriptNode[] = [];
  const compactPending = () => {
    const tools = pending.filter((n) => n.type === "tool");
    const thoughts = pending.filter((n) => n.type === "thought");
    const rest = pending.filter((n) => n.type !== "tool" && n.type !== "thought");
    if (tools.length + thoughts.length >= 2) {
      out.push({
        type: "compact",
        id: `compact:${tools[0]?.id ?? thoughts[0]?.id}`,
        toolCount: tools.length,
        thoughtCount: thoughts.length,
        children: pending.filter((n) => n.type === "tool" || n.type === "thought"),
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
