import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import type { TranscriptNode } from "../assemble";
import { supersedeStreamed } from "./supersede";

type Msg = Extract<TranscriptNode, { type: "message" }>;

function assistant(id: string, text: string): Msg {
  return { type: "message", id, role: "assistant", text, status: "complete", origin: "human" };
}

function messageEvent(id: string, channel: "hook" | "transcript", seq: number): Observation {
  return {
    kind: "message",
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: "2026-09-16T00:00:00.000Z",
    nativeAt: { state: "not-applicable" },
    source: {
      adapterVersion: "t",
      channel,
      delivery: "live",
      driverKind: "shell-pty",
      driverVersion: "t",
      nativeAgentId: { state: "not-applicable" },
      nativeEventId: { state: "not-applicable" },
      nativeItemId: { state: "not-applicable" },
      nativeRequestId: { type: "none" },
      nativeSessionId: { state: "known", value: "s" },
      nativeTurnId: { state: "not-applicable" },
      sourceCursor: { type: "runtime", value: { ledgerRevision: String(seq) } },
    },
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      nodeId: id,
      messageId: id,
      revision: "1",
      baseRevision: null,
      operation: "close",
      role: "assistant",
      phase: "final",
      blocks: [],
      targetBlock: null,
      parentToolCallId: null,
      status: "complete",
      nativeOrigin: { state: "known", value: id },
    },
  } as unknown as Observation;
}

describe("supersedeStreamed", () => {
  it("collapses a confirmed streamed bubble onto the transcript node, in the streamed slot", () => {
    const nodes: TranscriptNode[] = [
      assistant("stream-1", "line one\nline two"),
      assistant("msg_vrtx_1", "line one\nline two\nline three"),
    ];
    const events = [messageEvent("stream-1", "hook", 1), messageEvent("msg_vrtx_1", "transcript", 2)];

    const out = supersedeStreamed(nodes, events);
    expect(out).toHaveLength(1);
    // The transcript node takes the streamed node's slot: the row upgrades in
    // place instead of moving, so the reader's scroll position is unchanged.
    expect(out[0]?.id).toBe("msg_vrtx_1");
    expect(out[0]).toHaveProperty("text", "line one\nline two\nline three");
  });

  it("keeps a streamed node no transcript message confirms, with its partial chrome", () => {
    const nodes: TranscriptNode[] = [
      { ...assistant("stream-1", "client error echo"), status: "streaming" },
    ];
    const events = [messageEvent("stream-1", "hook", 1)];
    const out = supersedeStreamed(nodes, events);
    expect(out).toHaveLength(1);
    expect(out[0]?.id).toBe("stream-1");
  });

  it("does not supersede when the transcript text is not a prefix extension", () => {
    const nodes: TranscriptNode[] = [
      assistant("stream-1", "completely different streamed text"),
      assistant("msg_vrtx_1", "the authoritative answer"),
    ];
    const events = [messageEvent("stream-1", "hook", 1), messageEvent("msg_vrtx_1", "transcript", 2)];
    const out = supersedeStreamed(nodes, events);
    expect(out).toHaveLength(2);
  });

  it("is whitespace tolerant but text-sensitive", () => {
    const nodes: TranscriptNode[] = [
      assistant("stream-1", "  hello   world "),
      assistant("msg_vrtx_1", "hello world\n\nmore"),
    ];
    const events = [messageEvent("stream-1", "hook", 1), messageEvent("msg_vrtx_1", "transcript", 2)];
    expect(supersedeStreamed(nodes, events)).toHaveLength(1);
  });

  it("never pairs two streams or two transcripts, and never touches user nodes", () => {
    const user = {
      type: "message" as const,
      id: "user-1",
      role: "user" as const,
      text: "stream-1",
      status: "complete",
      origin: "human" as const,
    };
    const nodes: TranscriptNode[] = [user, assistant("stream-1", "stream-1"), assistant("stream-2", "stream-2")];
    const events = [
      messageEvent("user-1", "transcript", 1),
      messageEvent("stream-1", "hook", 2),
      messageEvent("stream-2", "hook", 3),
    ];
    const out = supersedeStreamed(nodes, events);
    expect(out).toHaveLength(3);
  });

  it("returns the same array reference when nothing collapses (memo identity)", () => {
    const nodes: TranscriptNode[] = [assistant("msg_vrtx_1", "only transcript")];
    const out = supersedeStreamed(nodes, [messageEvent("msg_vrtx_1", "transcript", 1)]);
    expect(out).toBe(nodes);
  });
});

describe("supersedeStreamed — tool convergence", () => {
  const toolNode = (
    id: string,
    name: string,
    input: unknown,
    result: unknown = null,
  ): TranscriptNode =>
    ({
      type: "tool",
      id,
      family: "Bash",
      name,
      driverKind: "shell-pty",
      call: { toolCallId: id, toolName: { state: "known", value: name }, input: { state: "known", value: input } },
      result,
      completeness: "structured",
      diffState: "unknown",
    }) as unknown as TranscriptNode;

  const toolEvent = (id: string, channel: "hook" | "transcript", seq: number): Observation =>
    ({
      kind: "tool_call",
      seq: String(seq),
      observedAt: "2026-09-16T00:00:00.000Z",
      eventId: `ev_${seq}`,
      source: { channel },
      payload: { toolCallId: id },
    }) as unknown as Observation;

  it("keeps the hook running card until Final: it owns the live anchor and the ticker", () => {
    const input = { command: "sleep 15" };
    const nodes: TranscriptNode[] = [
      toolNode("obj-hook", "Bash", input),
      toolNode("obj-script", "Bash", input),
    ];
    const out = supersedeStreamed(nodes, [toolEvent("obj-hook", "hook", 1), toolEvent("obj-script", "transcript", 2)]);
    expect(out).toHaveLength(1);
    // No Final anywhere yet: the hook card survives in its own slot so the
    // running dot and elapsed keep their element identity.
    expect(out[0]?.id).toBe("obj-hook");
  });

  it("keeps two different tool calls apart", () => {
    const nodes: TranscriptNode[] = [
      toolNode("obj-a", "Bash", { command: "a" }),
      toolNode("obj-b", "Bash", { command: "b" }),
    ];
    const out = supersedeStreamed(nodes, [toolEvent("obj-a", "hook", 1), toolEvent("obj-b", "hook", 2)]);
    expect(out).toHaveLength(2);
  });

  it("takes the transcript identity at Final but keeps the hook result's exit code and output", () => {
    const input = { command: "echo done" };
    const hookResult = {
      stage: "final",
      outcome: "succeeded",
      exitCode: { state: "known", value: 0 },
      structuredResult: { state: "unknown", reason: "x" },
      blocks: [{ type: "text", text: "done" }],
      changes: [],
    };
    const scriptResult = {
      stage: "final",
      outcome: "succeeded",
      exitCode: { state: "unknown", reason: "not-emitted" },
      structuredResult: { state: "unknown", reason: "x" },
      blocks: [],
      changes: [],
    };
    const nodes: TranscriptNode[] = [
      toolNode("obj-hook", "Bash", input, hookResult),
      toolNode("obj-script", "Bash", input, scriptResult),
    ];
    const out = supersedeStreamed(nodes, [toolEvent("obj-hook", "hook", 1), toolEvent("obj-script", "transcript", 2)]);
    expect(out).toHaveLength(1);
    // The row upgrades to the authoritative transcript identity in place…
    expect(out[0]?.id).toBe("obj-script");
    const result = (out[0] as Extract<TranscriptNode, { type: "tool" }>).result;
    // …but the hook tier's known exit code and text come along.
    expect(result?.exitCode).toEqual({ state: "known", value: 0 });
    expect(result?.blocks[0]).toEqual({ type: "text", text: "done" });
  });
});
