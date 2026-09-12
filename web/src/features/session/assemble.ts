import type {
  Observation,
  ThoughtPayload,
  ToolCallPayload,
  ToolResultPayload,
  UsagePayload,
  WorkflowMemberPayload,
  WorkflowRunPayload,
} from "../../types/observation";
import type { Interaction } from "../../types/interaction";
import { knowledgeValue } from "../../types/command";
import { familyFor, type ToolFamily } from "./toolRegistry";
import { observationText } from "../../lib/api";

export type DiffState = "proposed" | "applied" | "unknown";

export type ToolNode = {
  type: "tool";
  id: string;
  family: ToolFamily;
  name: string;
  call: ToolCallPayload;
  result: ToolResultPayload | null;
  completeness: Observation["completeness"];
  diffState: DiffState;
};

export type TranscriptNode =
  | { type: "message"; id: string; role: "user" | "assistant" | "system"; text: string; status: string }
  | { type: "thought"; id: string; text: string; completeness: Observation["completeness"] }
  | ToolNode
  | { type: "workflow"; id: string; run: WorkflowRunPayload; members: WorkflowMemberPayload[] }
  | { type: "usage"; id: string; payload: UsagePayload }
  | { type: "interaction"; id: string; interaction: Interaction }
  | { type: "opaque"; id: string; kind: string }
  | { type: "compact"; id: string; toolCount: number; thoughtCount: number; children: TranscriptNode[] };

function diffState(call: ToolCallPayload, result: ToolResultPayload | null, completeness: Observation["completeness"]): DiffState {
  if (completeness === "partial" && !result) return "unknown";
  if (!result) return call.state === "proposed" ? "proposed" : "unknown";
  const app = result.changes[0]?.application;
  if (app === "applied") return "applied";
  if (app === "proposed") return "proposed";
  if (result.changes.length === 0) return "unknown";
  return "unknown";
}

export function assembleTranscript(events: Observation[]): TranscriptNode[] {
  const tools = new Map<string, ToolNode>();
  const workflows = new Map<string, { type: "workflow"; id: string; run: WorkflowRunPayload; members: WorkflowMemberPayload[] }>();
  const nodes: TranscriptNode[] = [];

  const pushTool = (node: ToolNode) => {
    tools.set(node.call.toolCallId, node);
    if (!nodes.some((n) => n.type === "tool" && n.call.toolCallId === node.call.toolCallId)) nodes.push(node);
  };

  for (const ev of events) {
    if (ev.kind === "message") {
      const text = observationText(ev);
      nodes.push({
        type: "message",
        id: ev.eventId,
        role: (ev.payload as { role: "user" | "assistant" | "system" }).role,
        text,
        status: (ev.payload as { status: string }).status,
      });
      continue;
    }
    if (ev.kind === "thought") {
      const payload = ev.payload as ThoughtPayload;
      nodes.push({
        type: "thought",
        id: ev.eventId,
        text: payload.text ?? "",
        completeness: ev.completeness,
      });
      continue;
    }
    if (ev.kind === "tool_call") {
      const call = ev.payload as ToolCallPayload;
      const name = knowledgeValue(call.toolName) ?? "tool";
      const existing = tools.get(call.toolCallId);
      const node: ToolNode = {
        type: "tool",
        id: call.toolCallId,
        family: familyFor(ev.source.driverKind, name),
        name,
        call,
        result: existing?.result ?? null,
        completeness: ev.completeness,
        diffState: diffState(call, existing?.result ?? null, ev.completeness),
      };
      if (existing) {
        existing.call = call;
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
      const result = ev.payload as ToolResultPayload;
      const existing = tools.get(result.toolCallId);
      if (existing) {
        existing.result = result;
        existing.diffState = diffState(existing.call, result, ev.completeness);
        if (ev.completeness === "partial") existing.completeness = "partial";
      }
      continue;
    }
    if (ev.kind === "workflow.run") {
      const run = ev.payload as WorkflowRunPayload;
      const current = workflows.get(run.workflowId) ?? {
        type: "workflow" as const,
        id: run.workflowId,
        run,
        members: [],
      };
      current.run = run;
      workflows.set(run.workflowId, current);
      if (!nodes.includes(current)) nodes.push(current);
      continue;
    }
    if (ev.kind === "workflow.member") {
      const member = ev.payload as WorkflowMemberPayload;
      const current = workflows.get(member.workflowId);
      if (current) {
        current.members = current.members.filter((m) => m.memberId !== member.memberId).concat(member);
      }
      continue;
    }
    if (ev.kind === "usage") {
      nodes.push({ type: "usage", id: ev.eventId, payload: ev.payload as UsagePayload });
      continue;
    }
    if (ev.kind === "interaction.requested") {
      nodes.push({
        type: "interaction",
        id: ev.eventId,
        interaction: (ev.payload as { interaction: Interaction }).interaction,
      });
      continue;
    }
    if (ev.kind === "lifecycle") continue;
    if (ev.kind === "interaction.answered" || ev.kind === "interaction.expired") continue;
    nodes.push({ type: "opaque", id: ev.eventId, kind: ev.kind });
  }

  return nodes;
}

export function compactTranscript(nodes: TranscriptNode[], enabled: boolean): TranscriptNode[] {
  if (!enabled) return nodes;
  const out: TranscriptNode[] = [];
  let pending: TranscriptNode[] = [];
  const flushPending = () => {
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
      flushPending();
      out.push(node);
      continue;
    }
    if (node.type === "message" && node.role === "assistant") {
      flushPending();
      out.push(node);
      continue;
    }
    if (node.type === "usage" || node.type === "interaction") {
      flushPending();
      out.push(node);
      continue;
    }
    pending.push(node);
  }
  flushPending();
  return out;
}
