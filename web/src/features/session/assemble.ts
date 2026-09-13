import type {
  ContentBlock,
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
  | { type: "message"; id: string; role: "user" | "assistant" | "system"; text: string; status: string; local?: LocalBubble }
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
  if (block?.type !== "text" || next.some((delta) => delta.type !== "text")) return current;
  const blocks = current.slice();
  blocks[target] = { type: "text", text: block.text + next.map((delta) => delta.type === "text" ? delta.text : "").join("") };
  return blocks;
}

function blocksText(blocks: ContentBlock[]): string {
  return blocks.flatMap((block) => block.type === "text" && block.text ? [block.text] : []).join("\n");
}

export function assembleTranscript(events: Observation[], bubbles: LocalBubble[] = []): TranscriptNode[] {
  const messages = new Map<string, { mutation: NodeMutation; blocks: ContentBlock[]; node: Extract<TranscriptNode, { type: "message" }> }>();
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

  for (const ev of events) {
    if (ev.kind === "message") {
      const payload = ev.payload;
      const existing = messages.get(`message:${payload.messageId}`) ?? messages.get(`node:${payload.nodeId}`);
      if (!newerMutation(payload, existing?.mutation)) continue;
      const blocks = existing ? messageBlocks(existing.blocks, payload.blocks, payload.operation, payload.targetBlock) : payload.blocks;
      const node = existing?.node ?? { type: "message" as const, id: payload.messageId, role: payload.role, text: "", status: payload.status };
      node.text = blocksText(blocks);
      node.role = payload.role;
      node.status = payload.status;
      const current = existing ?? { mutation: payload, blocks, node };
      current.mutation = payload;
      current.blocks = blocks;
      messages.set(`message:${payload.messageId}`, current);
      messages.set(`node:${payload.nodeId}`, current);
      if (!existing) nodes.push(node);
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
      if (!nodes.includes(current)) nodes.push(current);
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
    if (node.type === "message" && node.role === "user") seenUser.add(node.text);
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
