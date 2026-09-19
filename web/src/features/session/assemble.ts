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
import type { Id } from "../../types/wire";
import { knowledgeValue } from "../../types/command";
import { familyFor, type ToolFamily } from "./toolRegistry";
import type { LocalBubble } from "../../lib/store";
import { supersedeStreamed } from "./live/supersede";
import { mountLiveThinking } from "./live/liveThinking";

export type DiffState = "proposed" | "applied" | "unknown";

/**
 * One subagent's folded activity under a parent row.
 *
 * Subagents (Workflow members and plain Agent/Task tasks) are Claude
 * sub-sessions inside the SAME Remuda session — never Remuda instances — so
 * their own tool calls must not be flattened into the main transcript. The
 * assembler folds every tool observation whose `nativeAgentId` names a
 * subagent into that ref, keyed by the native agent id:
 *
 * - workflow members join via `workflow.member.nativeAgentId`;
 * - plain tasks join via the Task launch's `parentToolCallId` stamped by the
 *   hook fold (background agentId on the launch result, foreground bound at
 *   SubagentStart).
 */
export type SubagentRef = {
  /** Native agent id; also the drill-in route key. */
  agentId: string;
  /** "workflow-member" rows live inside the workflow card; "agent" under Task. */
  kind: "workflow-member" | "agent";
  /** The subagent's own tool rows, in journal order. */
  nodes: ToolNode[];
};

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
   * Native agent id when THIS tool observation was made by a subagent (its
   * hook events carried `agent_id`). Such a node is folded under its parent
   * instead of staying at the top level.
   */
  agentId?: string;
  /**
   * Folded subagent activity for a plain Task/Agent parent row. Workflow
   * members instead ride {@link ToolNode.workflow}.
   */
  subagents?: SubagentRef[];
  /**
   * r-ux-w: workflow timeline data mounted on this tool row when the
   * `workflow.run` observation named this tool call. Batch W owns
   * WorkflowTimelineCard; this hook is the only seam into the E-owned file.
   */
  workflow?: {
    run: WorkflowRunPayload;
    phases: WorkflowPhasePayload[];
    members: WorkflowMemberPayload[];
    /** c-wfdrill: per-member folded live tool rows. */
    subagents?: SubagentRef[];
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
      /**
       * Local-only enrichment merged from the optimistic bubble that joined
       * this journal node by commandId (C2). The journal never echoes the
       * staged attachment thumbnails (D-027) back, so they ride in here
       * WITHOUT marking the whole node `local` — `local` would mis-render the
       * authoritative journal node as an optimistic bubble (withdraw button,
       * optimistic testid).
       */
      localAttachments?: LocalBubble["attachments"];
      /**
       * Server command that delivered this prompt (C2). Present only on human
       * user nodes that came through a Remuda command; natively typed prompts
       * leave it unset.
       */
      commandId?: Id;
      /**
       * c-steer: when the row is a Remuda-held queue row (not yet POSTed), why
       * it waits and its 1-based position among turn-wait held rows.
       */
      holdReason?: "turn" | "answer";
      holdOrdinal?: number;
      /** c-steer: a delivered 插队 (promptMode carried by the Node journal). */
      promptMode?: string;
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

/**
 * Node-scoped mutation baseline (protocol.md §5.2).
 *
 * Revisions are per NODE — "append/replace/close 的 revision 必须是当前 node
 * revision+1，baseRevision 必须匹配" — and the driver sequences every
 * mutation on a node (the call track AND its result track) through that one
 * counter: grok's Proposed Open is 1, the Running Replace is 2, a streamed
 * Partial Append is 3, another progress Replace is 4, and the Final Close is
 * 5+. A per-track baseline therefore rejects the Partial whose base is the
 * last *call* revision.
 *
 * The slot is (nodeId, toolCallId) rather than nodeId alone: grok journals
 * call and result on the same node (nodeId == toolCallId), so both tracks
 * share one slot here; producers that keep the result on its own node (the
 * claude `tool_result_id` shape) get the independent counter the per-track
 * code used to keep.
 *
 * The promoted-claude print path closes the call and the result on the SAME
 * node at the SAME revision (call close rev 2, then result close rev 2): the
 * two tracks legitimately coincide there. Track which kind of mutation holds
 * the revision so that cross-track Close is applied once but a re-delivered
 * result Close is treated as the duplicate it is — otherwise a replay with
 * a thinner payload would overwrite the exit-bearing Final.
 */
class NodeBaselines {
  private readonly held = new Map<string, { revision: string; track: "call" | "result" }>();

  private static key(nodeId: string, toolCallId: string): string {
    return `${nodeId} ${toolCallId}`;
  }

  /**
   * Whether `next` may apply on top of the held node baseline. Snapshots
   * (replace/close) need a strictly newer revision; an append needs its exact
   * base, because its blocks carry just the new content. A Close at the SAME
   * revision is accepted only while that revision is held by the *call*
   * track (the cross-track promoted-claude Close), never once a result
   * already holds it (so re-deliveries cannot overwrite the Final).
   */
  accepts(
    next: NodeMutation,
    toolCallId: string,
    track: "call" | "result",
  ): boolean {
    const current = this.held.get(NodeBaselines.key(next.nodeId, toolCallId));
    if (!current) return true;
    const revision = BigInt(next.revision);
    const baseline = BigInt(current.revision);
    if (revision < baseline) return false;
    if (revision === baseline) {
      return next.operation === "close" && track === "result" && current.track === "call";
    }
    return next.operation !== "append" || next.baseRevision === current.revision;
  }

  set(nodeId: string, toolCallId: string, revision: string, track: "call" | "result"): void {
    this.held.set(NodeBaselines.key(nodeId, toolCallId), { revision, track });
  }
}

/**
 * Fold one tool-result mutation into the card's accumulated result.
 *
 * Protocol.md §5.2 gives the result stream the same contract as messages:
 * - `append` blocks carry only the NEW content (grok's terminal log streams
 *   raw byte fragments with no block target), so text fragments concatenate
 *   with NO separator into one text block — joining blocks later with "\n"
 *   would double every embedded newline;
 * - `replace` is the complete current value (a rotated log republishes as a
 *   snapshot), so it resets the accumulated text;
 * - `close` is the authoritative native item: it replaces the streamed
 *   prefix outright and the Partial bytes are never concatenated into it.
 */
function mergeToolResult(current: ToolResultPayload | null, next: ToolResultPayload): ToolResultPayload {
  if (next.operation !== "append" || !current) return next;
  if (next.blocks.length === 0) return { ...next, blocks: current.blocks };
  const appended = next.blocks
    .flatMap((block) => (block.type === "text" ? [block.text] : []))
    .join("");
  const blocks = current.blocks.slice();
  if (appended) {
    const last = blocks.length - 1;
    if (blocks[last]?.type === "text") {
      blocks[last] = { type: "text", text: blocks[last].text + appended };
    } else {
      blocks.push({ type: "text", text: appended });
    }
  }
  // New non-text blocks (image/file/resource/opaque) are kept after the
  // streamed text; the Partial envelope's other facts (stage, outcome,
  // revision) ride the merge.
  for (const block of next.blocks) {
    if (block.type !== "text") blocks.push(block);
  }
  return { ...next, blocks };
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
      // C2: carry the delivering command id so the UI can attribute the
      // node and hide the matching optimistic bubble.
      if (payload.commandId != null) node.commandId = payload.commandId;
      // c-steer: keep the delivery mode of the latest revision (a queued row
      // that completes as a 插队 keeps the 插队 badge).
      if (payload.promptMode != null) node.promptMode = payload.promptMode;
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
  // One applied revision per node, shared by the call and result tracks
  // (protocol.md §5.2); see NodeBaselines.
  const toolBaselines = new NodeBaselines();
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
      if (!toolBaselines.accepts(call, call.toolCallId, "call")) continue;
      toolBaselines.set(call.nodeId, call.toolCallId, call.revision, "call");
      const agentId = knowledgeValue(ev.source.nativeAgentId) ?? existing?.agentId;
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
        ...(agentId ? { agentId } : {}),
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
        if (agentId) existing.agentId = agentId;
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
        if (!toolBaselines.accepts(result, result.toolCallId, "result")) continue;
        toolBaselines.set(result.nodeId, result.toolCallId, result.revision, "result");
        existing.result = mergeToolResult(existing.result, result);
        existing.diffState = diffState(existing.call, existing.result, ev.completeness);
        // A streamed Partial marks the card incomplete (ui-spec §3.3); the
        // structured Final settles it as a normal card — the latch must not
        // survive the Close that just replaced the streamed prefix.
        existing.completeness = ev.completeness;
      }
      continue;
    }
    if (ev.kind === "workflow.run" || ev.kind === "workflow.phase" || ev.kind === "workflow.member") {
      // Keep the per-workflow accumulator (mutated in place so a card already
      // mounted on a tool row stays live), then either mount it on the
      // Workflow tool row (r-ux-w decision 1) or keep a standalone node when
      // the producer gave no tool call id to attach it to.
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
        anchor(current, ev);
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
    // C2: a journal node carrying the same commandId is the authoritative
    // copy of this optimistic bubble — the Node joined hook/transcript
    // evidence onto the delivering command. The Node journals its own queued
    // observation as soon as the command is enqueued, which can beat the HTTP
    // response, so this join happens even while the bubble is still `queued`.
    //
    // The bubble still owns a fact the journal never echoes back: the local
    // attachment thumbnails (D-027). Carry them onto the joined node in place
    // so the rendered row keeps them instead of dropping them when the
    // optimistic bubble is hidden.
    if (bubble.commandId) {
      const joinedIndex = nodes.findIndex(
        (node) =>
          node.type === "message" &&
          node.role === "user" &&
          node.commandId === bubble.commandId,
      );
      if (joinedIndex >= 0) {
        const joined = nodes[joinedIndex];
        if (joined.type === "message") {
          // Carry the bubble's local-only attachment thumbnails onto the
          // authoritative journal node (the journal never echoes them back),
          // but leave `local` unset so the node does not render as an
          // optimistic bubble.
          nodes[joinedIndex] = {
            ...joined,
            localAttachments: joined.localAttachments ?? bubble.attachments,
          };
        }
        continue;
      }
    } else {
      // No server id (pre-C2 producers / POST failure): keep the legacy text
      // rule, which never hides an in-flight queued bubble.
      if (bubble.state !== "queued" && seenUser.has(bubble.text)) continue;
    }
    // c-steer: 1-based position among turn-wait held rows, in queue order.
    let holdOrdinal: number | undefined;
    if (bubble.held && bubble.holdReason === "turn") {
      holdOrdinal =
        bubbles
          .slice(0, bubbles.indexOf(bubble))
          .filter((b) => b.held && b.state === "queued" && b.holdReason !== "answer").length + 1;
    }
    nodes.push({
      type: "message",
      // The node's local identity is the pre-POST clientRequestId.
      id: bubble.clientRequestId,
      role: "user",
      text: bubble.text,
      status: bubble.state,
      // A local bubble is text this user just typed into the composer.
      origin: "human",
      local: bubble,
      commandId: bubble.commandId ?? undefined,
      ...(bubble.held ? { holdReason: bubble.holdReason ?? "turn", holdOrdinal } : {}),
    });
  }

  // Hook-streamed assistant bubbles are display echoes: collapse them onto
  // the authoritative transcript message in the projection (design §2.3).
  // The screen-tier "thinking" hint is the one other live mount: a collapsed
  // thought row while the spinner phrase says reasoning is in progress.
  // Subagent tool activity is then folded UNDER the Task / workflow-member
  // parent instead of being flattened into the main transcript.
  return groupSubagentTools(supersedeStreamed(mountLiveThinking(nodes, events), events));
}

/**
 * Move every top-level tool node a subagent produced under its parent row.
 *
 * Identity, in priority order:
 * 1. the typed `parentToolCallId` the hook fold stamps on a subagent's tool
 *    call (plain Task/Agent launch — background agentId is known at the launch
 *    result, foreground binds at SubagentStart);
 * 2. a workflow member's `nativeAgentId`, which joins the Workflow tool row
 *    the run is mounted on.
 *
 * A node whose agent matches neither stays top-level: grouping is an
 * evidence-backed projection, never a guess.
 */
export function groupSubagentTools(nodes: TranscriptNode[]): TranscriptNode[] {
  const topTools = new Map<string, ToolNode>();
  for (const node of nodes) {
    if (node.type === "tool") topTools.set(node.id, node);
  }
  // agentId -> Workflow tool row, from mounted member identity.
  const workflowParent = new Map<string, ToolNode>();
  for (const node of topTools.values()) {
    for (const member of node.workflow?.members ?? []) {
      const id = knowledgeValue(member.nativeAgentId);
      if (id) workflowParent.set(id, node);
    }
  }

  const removed = new Set<ToolNode>();
  for (const node of nodes) {
    if (node.type !== "tool" || !node.agentId) continue;
    const linkedParent = node.call.parentToolCallId
      ? topTools.get(node.call.parentToolCallId)
      : undefined;
    const memberParent = workflowParent.get(node.agentId);
    const parent = linkedParent ?? memberParent;
    if (!parent || parent === node) continue;
    const kind: SubagentRef["kind"] = linkedParent ? "agent" : "workflow-member";
    if (kind === "agent") {
      const holder = (parent.subagents ??= []);
      let bucket = holder.find((entry) => entry.agentId === node.agentId);
      if (!bucket) {
        bucket = { agentId: node.agentId, kind, nodes: [] };
        holder.push(bucket);
      }
      bucket.nodes.push(node);
    } else if (parent.workflow) {
      const holder = (parent.workflow.subagents ??= []);
      let bucket = holder.find((entry) => entry.agentId === node.agentId);
      if (!bucket) {
        bucket = { agentId: node.agentId, kind, nodes: [] };
        holder.push(bucket);
      }
      bucket.nodes.push(node);
    }
    removed.add(node);
  }
  if (removed.size === 0) return nodes;
  return nodes.filter((node) => !(node.type === "tool" && removed.has(node)));
}

/** Every tool row nested under a parent (Task children / workflow members). */
export function subagentToolNodes(node: TranscriptNode): ToolNode[] {
  if (node.type !== "tool") return [];
  const refs = [...(node.subagents ?? []), ...(node.workflow?.subagents ?? [])];
  return refs.flatMap((ref) => ref.nodes);
}

/**
 * A tool whose own result says it did not run successfully. `denied` is
 * included: a refused permission is exactly the kind of thing the reader
 * needs to see, not discover hidden behind a fold.
 */
export function isToolFailure(node: TranscriptNode): boolean {
  return node.type === "tool" && (node.result?.outcome === "failed" || node.result?.outcome === "denied");
}

/**
 * A tool row carrying a live workflow timeline card that the reader has NOT
 * dismissed. Such a row keeps its inline position exactly like a failed tool:
 * a running or finished workflow must stay a visible live card, and only
 * per-workflow dismissal (c-wfcard) lets it join the compact fold.
 */
export function isKeptWorkflow(node: TranscriptNode, dismissedWorkflowIds?: ReadonlySet<string>): boolean {
  if (node.type !== "tool" || !node.workflow) return false;
  return !dismissedWorkflowIds?.has(node.workflow.run.workflowId);
}

/** Compact only after a turn ends (assistant or usage). In-flight tools stay expanded. */
export function compactTranscript(
  nodes: TranscriptNode[],
  enabled: boolean,
  dismissedWorkflowIds?: ReadonlySet<string>,
): TranscriptNode[] {
  if (!enabled) return nodes;
  // Failed tools and undismissed workflow cards keep their inline position.
  const staysOutside = (n: TranscriptNode) => isToolFailure(n) || isKeptWorkflow(n, dismissedWorkflowIds);
  const out: TranscriptNode[] = [];
  let pending: TranscriptNode[] = [];
  const compactPending = () => {
    // Failed tools and undismissed workflow cards stay in `rest`; the fold
    // only swallows routine tools and thoughts.
    const tools = pending.filter((n) => n.type === "tool" && !staysOutside(n));
    const thoughts = pending.filter((n) => n.type === "thought");
    const rest = pending.filter((n) => (n.type !== "tool" || staysOutside(n)) && n.type !== "thought");
    if (tools.length + thoughts.length >= 2) {
      out.push({
        type: "compact",
        id: `compact:${tools[0]?.id ?? thoughts[0]?.id}`,
        toolCount: tools.length,
        thoughtCount: thoughts.length,
        children: pending.filter((n) => (n.type === "tool" && !staysOutside(n)) || n.type === "thought"),
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
