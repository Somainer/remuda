import { describe, expect, it } from "vitest";
import type { MessagePayload, Observation, ToolCallPayload } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import { assembleTranscript, compactTranscript, diffState, isToolFailure, type TranscriptNode } from "./assemble";

function obs(seq: number, kind: Observation["kind"], payload: unknown): Observation {
  return {
    schemaVersion: 1,
    eventId: `evt_${seq}` as Id,
    journalId: "obj_j" as Id,
    instanceId: "ins" as Id,
    runId: null,
    hostId: "hst" as Id,
    processGeneration: "1",
    runGeneration: null,
    seq: String(seq),
    observedAt: "2026-09-12T00:00:00.000Z",
    nativeAt: known("2026-09-12T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "1",
      adapterVersion: "1",
      channel: "stdout",
      delivery: "replay",
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind,
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

function call(name: string, toolCallId: string): ToolCallPayload {
  return {
    nodeId: "n" as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: toolCallId as Id,
    parentToolCallId: null,
    toolName: known(name),
    displayTitle: known(name),
    category: "other",
    input: known({}),
    inputTextDelta: null,
    state: "running",
    executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
  };
}

function message(seq: number, text: string, extra: Partial<MessagePayload> = {}): Observation {
  return obs(seq, "message", {
    nodeId: "message-node", messageId: "message", role: "assistant", phase: "final",
    revision: String(seq), baseRevision: seq === 1 ? null : String(seq - 1),
    operation: seq === 1 ? "open" : "append", status: "streaming",
    blocks: [{ type: "text", text }], targetBlock: 0,
    parentToolCallId: null, nativeOrigin: known("assistant"), ...extra,
  });
}

describe("assembleTranscript", () => {
  it("reconstructs every arrival order of open, targeted append, and empty close", () => {
    const open = message(1, "hello");
    const append = message(2, " world");
    const close = message(3, "", { operation: "close", blocks: [], status: "complete" });
    const orders = [
      [open, append, close], [open, close, append], [append, open, close],
      [append, close, open], [close, open, append], [close, append, open],
    ];
    for (const events of orders) {
      expect(assembleTranscript(events)).toMatchObject([
        { id: "message-node", text: "hello world", status: "complete" },
      ]);
      expect(assembleTranscript([...events, append, open, close])).toEqual(assembleTranscript(events));
    }
  });

  it.each(["append", "replace", "close"] as const)("renders a first %s without requiring an open", (operation) => {
    expect(assembleTranscript([message(8, "available text", { operation, status: "complete" })]))
      .toMatchObject([{ text: "available text", status: "complete" }]);
  });

  it("shows the latest available suffix until its exact prefix arrives", () => {
    const open = message(1, "a");
    const second = message(2, "b");
    const third = message(3, "c");
    expect(assembleTranscript([third])).toMatchObject([{ text: "c" }]);
    expect(assembleTranscript([third, open])).toMatchObject([{ text: "c" }]);
    expect(assembleTranscript([open, third])).toMatchObject([{ text: "c" }]);
    expect(assembleTranscript([open, third, second])).toMatchObject([{ text: "abc" }]);
  });

  it("keeps a retained suffix at its target block for subsequent appends", () => {
    const open = message(1, "", { blocks: [{ type: "text", text: "Header" }, { type: "text", text: "a" }] });
    const second = message(2, "b", { targetBlock: 1 });
    const third = message(3, "c", { targetBlock: 1 });
    const fourth = message(4, "d", { targetBlock: 1 });
    expect(assembleTranscript([fourth, third, open])).toMatchObject([{ text: "cd" }]);
    expect(assembleTranscript([fourth, third, open, second])).toMatchObject([{ text: "Header\nabcd" }]);
  });

  it("prefers equal-revision completion without replaying a duplicate delta", () => {
    const open = message(1, "hello");
    const append = message(2, " world");
    const close = message(3, "", { revision: "2", operation: "close", blocks: [], status: "complete" });
    expect(assembleTranscript([close, append, open, append]))
      .toMatchObject([{ text: "hello world", status: "complete" }]);
    const snapshot = message(4, "final", { revision: "1", operation: "replace", status: "complete" });
    for (const events of [[open, snapshot], [snapshot, open]]) {
      expect(assembleTranscript(events)).toMatchObject([{ text: "final", status: "complete" }]);
    }
    const completedAppend = message(4, " world", { revision: "2", status: "complete" });
    expect(assembleTranscript([completedAppend, append, open]))
      .toMatchObject([{ text: "hello world", status: "complete" }]);
  });

  it("keeps the highest u64 snapshot when older revisions arrive later", () => {
    const newest = message(3, "newest", { revision: "18446744073709551615", operation: "close", status: "complete" });
    const previous = message(2, "older", { revision: "18446744073709551614", operation: "replace" });
    expect(assembleTranscript([newest, message(1, "oldest"), previous, newest]))
      .toMatchObject([{ text: "newest", status: "complete" }]);
  });

  it("keeps one bubble through targeted deltas, replay, and complete snapshots", () => {
    const events = [
      message(1, "", { blocks: [{ type: "text", text: "Header" }, { type: "text", text: "我" }] }),
      message(2, "先", { targetBlock: 1 }),
      message(3, "看看", { targetBlock: 1 }),
    ];
    expect(assembleTranscript(events)).toMatchObject([{ id: "message-node", text: "Header\n我先看看", status: "streaming" }]);
    events.push(events[1], message(4, "", {
      operation: "replace", targetBlock: null,
      blocks: [{ type: "text", text: "Header" }, { type: "text", text: "我先看看仓库。" }],
    }), message(5, "", { operation: "close", blocks: [], status: "complete" }), events[2]);
    expect(assembleTranscript(events)).toMatchObject([{ id: "message-node", text: "Header\n我先看看仓库。", status: "complete" }]);
  });

  it("uses node identity as a fallback, recovers snapshots across gaps, and accepts empty replacement", () => {
    expect(assembleTranscript([
      message(1, "first"),
      message(3, "missing-base"),
      message(4, "final", { operation: "close", messageId: "renamed", status: "complete" }),
    ])).toMatchObject([{ id: "message-node", text: "final", status: "complete" }]);
    expect(assembleTranscript([message(1, "first"), message(2, "", { operation: "replace", blocks: [] })]))
      .toMatchObject([{ text: "" }]);
  });

  it("leaves historical fragments with distinct identities separate", () => {
    expect(assembleTranscript([
      message(1, "我"),
      message(2, "先", { nodeId: "another-node", messageId: "another-message", operation: "open", revision: "1", baseRevision: null }),
    ]).filter((node) => node.type === "message").map((node) => node.text)).toEqual(["我", "先"]);
  });

  it("assembles thinking deltas without duplicating the final snapshot", () => {
    const thought = (seq: number, text: string, operation: "open" | "append" | "close") => obs(seq, "thought", {
      nodeId: "thinking-node", thoughtId: "thinking", revision: String(seq),
      baseRevision: seq === 1 ? null : String(seq - 1), operation,
      representation: "text", text, partIndex: 0, status: operation === "close" ? "complete" : "streaming",
    });
    const events = [thought(1, "Check", "open"), thought(2, " files", "append")];
    expect(assembleTranscript(events)).toMatchObject([{ type: "thought", id: "thinking", text: "Check files" }]);
    expect(assembleTranscript([...events, events[1], thought(3, "Check files", "close"), events[0]]))
      .toMatchObject([{ type: "thought", id: "thinking", text: "Check files" }]);
  });

  it("keeps streamed tool input and its separate result node in one card", () => {
    const initial = call("Bash", "call");
    const firstDelta = { ...initial, revision: "2", baseRevision: "1", operation: "append", input: unknownKnowledge("partial"), inputTextDelta: '{"command":' };
    const events = [obs(1, "tool_call", initial), obs(2, "tool_call", firstDelta), obs(3, "tool_call", {
      ...firstDelta, revision: "3", baseRevision: "2", inputTextDelta: '"pwd"}',
    })];
    expect(assembleTranscript([...events, events[1]])).toMatchObject([{ type: "tool", call: { inputTextDelta: '{"command":"pwd"}' } }]);
    events.push(obs(4, "tool_call", { ...initial, operation: "replace", revision: "4", baseRevision: "3", input: known({ command: "pwd" }) }),
      obs(5, "tool_call", { ...initial, operation: "close", revision: "5", baseRevision: "4", input: known({ command: "pwd" }) }),
      obs(6, "tool_result", {
        nodeId: "result-node", toolCallId: "call", revision: "1", baseRevision: null, operation: "open",
        blocks: [{ type: "text", text: "/workspace" }], stage: "final", outcome: "succeeded", changes: [],
        structuredResult: unknownKnowledge("text"), exitCode: known(0),
      }));
    expect(assembleTranscript(events)).toMatchObject([{ type: "tool", call: { input: known({ command: "pwd" }) }, result: { nodeId: "result-node", outcome: "succeeded" } }]);
  });

  it("pairs tool call/result and keeps opaque + workflow phase", () => {
    const events = [
      obs(1, "message", {
        nodeId: "n" as Id,
        revision: "1",
        operation: "open",
        baseRevision: null,
        messageId: "m" as Id,
        role: "user",
        phase: "input",
        blocks: [{ type: "text", text: "hi" }],
        targetBlock: null,
        parentToolCallId: null,
        nativeOrigin: known("ui"),
        status: "complete",
      }),
      obs(2, "tool_call", call("Bash", "c1")),
      obs(3, "tool_result", {
        nodeId: "n" as Id,
        revision: "1",
        operation: "close",
        baseRevision: null,
        toolCallId: "c1" as Id,
        stage: "final",
        outcome: "succeeded",
        blocks: [{ type: "text", text: "ok" }],
        structuredResult: unknownKnowledge("text"),
        exitCode: known(0),
        changes: [],
      }),
      obs(4, "workflow.run", {
        workflowId: "wf" as Id,
        engine: "claude-workflow",
        nativeRunId: known("wf_9f3"),
        nativeTaskId: unknownKnowledge("none"),
        toolCallId: null,
        state: "running",
        revision: "1",
        title: known("compile"),
        resultRef: null,
      }),
      obs(5, "workflow.phase", {
        workflowId: "wf" as Id,
        phaseId: "ph" as Id,
        nativePhaseId: known("compile"),
        label: known("compile"),
        state: "running",
        revision: "1",
        parentPhaseId: null,
      }),
      obs(6, "opaque", {
        nativeType: "rate_limit_event",
        reason: "unmapped-native",
        rawRef: {
          objectId: "o" as Id,
          offset: "0",
          length: "0",
          digest: "sha256:00",
          mediaType: "application/json",
          redaction: "none",
        },
        affects: [],
        summary: "rate_limit_event",
      }),
    ];
    const nodes = assembleTranscript(events);
    expect(nodes.map((n) => n.type)).toEqual(["message", "tool", "workflow", "opaque"]);
    const tool = nodes.find((n) => n.type === "tool");
    expect(tool?.type === "tool" && tool.result?.outcome).toBe("succeeded");
    const wf = nodes.find((n) => n.type === "workflow");
    expect(wf?.type === "workflow" && wf.phases[0]?.label.state === "known" && wf.phases[0].label.value).toBe("compile");
    const opaque = nodes.find((n) => n.type === "opaque");
    expect(opaque?.type === "opaque" && opaque.kind).toBe("rate_limit_event");
  });
});

describe("assembleTranscript · C2 commandId correlation", () => {
  const userMessage = (seq: number, text: string, commandId?: string) =>
    obs(seq, "message", {
      nodeId: `obj_n${seq}`,
      messageId: `obj_n${seq}`,
      revision: "3",
      baseRevision: "2",
      operation: "replace",
      role: "user",
      phase: "input",
      blocks: [{ type: "text", text }],
      targetBlock: null,
      parentToolCallId: null,
      nativeOrigin: known("ui"),
      origin: "human",
      status: "complete",
      ...(commandId ? { commandId } : {}),
    });

  const bubble = (
    text: string,
    extra: { commandId?: string | null; state?: string; attachments?: unknown } = {},
  ) =>
    ({
      clientRequestId: `local_${text.replace(/\W/g, "_")}` as Id,
      instanceId: "ins" as Id,
      text,
      commandId: extra.commandId === undefined ? null : (extra.commandId as Id | null),
      state: extra.state ?? "accepted",
      ...(extra.attachments ? { attachments: extra.attachments } : {}),
      createdAt: "2026-09-15T00:00:00.000Z",
    }) as never;

  it("joins the optimistic bubble's local-only attachments onto the commandId node", () => {
    const nodes = assembleTranscript([userMessage(1, "do it", "cmd_1")], [bubble("do it", { commandId: "cmd_1" })]);
    const users = nodes.filter((n): n is Extract<(typeof nodes)[number], { type: "message" }> => n.type === "message" && n.role === "user");
    expect(users).toHaveLength(1);
    // The single row is the authoritative journal node, not an optimistic
    // bubble (no `local`), so it keeps the journal testid and no withdraw button.
    expect(users[0]).toMatchObject({ id: "obj_n1", commandId: "cmd_1" });
    expect(users[0].local).toBeUndefined();
  });

  it("carries attachment thumbnails onto the joined commandId node (D-027 paste + send)", () => {
    // The journal never echoes attachment thumbnails back; they live only on
    // the optimistic bubble. Hiding the bubble must not lose them.
    const withImage = bubble("describe it", {
      commandId: "cmd_img",
      attachments: [{ objectId: "obj_1", name: "red.png", previewUrl: "blob:red", index: 1 }],
    });
    const nodes = assembleTranscript([userMessage(1, "describe it", "cmd_img")], [withImage]);
    const user = nodes.find(
      (n): n is Extract<(typeof nodes)[number], { type: "message" }> =>
        n.type === "message" && n.role === "user" && n.commandId === "cmd_img",
    );
    expect(user).toBeDefined();
    // Carried as local-only enrichment, not by flipping the node to local.
    expect(user!.local).toBeUndefined();
    expect(user!.localAttachments).toEqual([
      { objectId: "obj_1", name: "red.png", previewUrl: "blob:red", index: 1 },
    ]);
  });

  it("keeps both when the journal node carries no matching commandId", () => {
    const nodes = assembleTranscript([userMessage(1, "typed natively")], [bubble("my send", { commandId: "cmd_9" })]);
    expect(nodes.filter((n) => n.type === "message" && n.role === "user")).toHaveLength(2);
  });

  it("does not settle a server-id bubble on text equality alone", () => {
    // Identical text, but the journal node is unattributed (native typing):
    // the server-id bubble stays visible until its own commandId node lands.
    const nodes = assembleTranscript([userMessage(1, "same")], [bubble("same", { commandId: "cmd_7" })]);
    const users = nodes.filter((n): n is Extract<(typeof nodes)[number], { type: "message" }> => n.type === "message" && n.role === "user");
    expect(users).toHaveLength(2);
    expect(users.map((n) => n.commandId)).toEqual([undefined, "cmd_7"]);
  });

  it("renders one bubble while queued even after a commandId node exists", () => {
    const nodes = assembleTranscript([userMessage(1, "queued", "cmd_3")], [
      bubble("queued", { commandId: "cmd_3", state: "queued" }),
    ]);
    // The queued bubble is the in-flight copy; the joined node is its backing
    // evidence. Never two rows: the bubble wins while it is still queued.
    expect(nodes.filter((n) => n.type === "message" && n.role === "user")).toHaveLength(1);
  });

  it("keeps the legacy text rule for bubbles without a server id", () => {
    // POST failure: commandId null, but a pre-C2 producer later journals the
    // same text. The old dedup still applies.
    const nodes = assembleTranscript([userMessage(1, "legacy text")], [bubble("legacy text", { commandId: null })]);
    const users = nodes.filter((n): n is Extract<(typeof nodes)[number], { type: "message" }> => n.type === "message" && n.role === "user");
    expect(users).toHaveLength(1);
    expect(users[0].local).toBeUndefined();
  });
});

describe("compactTranscript", () => {
  it("folds thought+tools after the assistant turn, not while in flight", () => {
    const thought = { type: "thought" as const, id: "t", text: "x", completeness: "structured" as const };
    const tool = {
      type: "tool" as const,
      id: "c",
      family: "Bash" as const,
      name: "Bash",
      driverKind: "claude-print",
      call: call("Bash", "c"),
      result: null,
      completeness: "structured" as const,
      diffState: "unknown" as const,
    };
    const inflight = compactTranscript(
      [
        { type: "message", id: "u", role: "user", text: "q", status: "complete", origin: "human" },
        thought,
        tool,
      ],
      true,
    );
    expect(inflight.some((n) => n.type === "compact")).toBe(false);
    const done = compactTranscript(
      [
        { type: "message", id: "u", role: "user", text: "q", status: "complete", origin: "human" },
        thought,
        tool,
        { type: "message", id: "a", role: "assistant", text: "ok", status: "complete", origin: "human" },
      ],
      true,
    );
    expect(done.some((n) => n.type === "compact")).toBe(true);
    expect(done.some((n) => n.type === "message" && n.role === "assistant")).toBe(true);
  });
});

describe("diffState", () => {
  const base = call("Edit", "e");
  it("proposed / applied / unknown", () => {
    expect(diffState({ ...base, state: "proposed" }, null, "structured")).toBe("proposed");
    expect(
      diffState(base, {
        nodeId: "n" as Id,
        revision: "1",
        operation: "close",
        baseRevision: null,
        toolCallId: "e" as Id,
        stage: "final",
        outcome: "succeeded",
        blocks: [],
        structuredResult: unknownKnowledge("diff"),
        exitCode: { state: "not-applicable" },
        changes: [{ path: "a", diff: "", application: "applied" }],
      }, "structured"),
    ).toBe("applied");
    expect(diffState(base, null, "partial")).toBe("unknown");
  });
});

describe("stable content identity (batch E)", () => {
  function result(seq: number, toolCallId: string, outcome: "succeeded" | "failed" | "denied"): Observation {
    return obs(seq, "tool_result", {
      nodeId: `result-${toolCallId}` as Id,
      toolCallId: toolCallId as Id,
      revision: "1",
      baseRevision: null,
      operation: "close",
      stage: "final",
      outcome,
      blocks: [{ type: "text", text: outcome }],
      structuredResult: unknownKnowledge("text"),
      exitCode: known(outcome === "succeeded" ? 0 : 1),
      changes: [],
    });
  }

  it("keeps the same node id through open, streaming appends and close", () => {
    const open = message(1, "hello", { nodeId: "stable-node", messageId: "stable-message" });
    const append = message(2, " world", { nodeId: "stable-node", messageId: "stable-message" });
    const close = message(3, "", {
      nodeId: "stable-node", messageId: "stable-message",
      operation: "close", blocks: [], status: "complete",
    });
    const first = assembleTranscript([open]);
    const mid = assembleTranscript([open, append]);
    const done = assembleTranscript([open, append, close]);
    expect(first[0]?.id).toBe("stable-node");
    expect(mid[0]?.id).toBe(first[0]?.id);
    expect(done[0]?.id).toBe(first[0]?.id);
  });

  it("holds the node id when the producer regroups (messageId renamed on close)", () => {
    // Suffix seen first: the regroup/close is the only event at first mount.
    const suffix = message(5, "final text", {
      nodeId: "same-node", messageId: "renamed",
      revision: "3", baseRevision: "2", operation: "close", status: "complete",
    });
    expect(assembleTranscript([suffix])[0]?.id).toBe("same-node");
    // Prefix/history arrive later via backfill and end up at the tail of the
    // store array; identity and text must reconcile without a key change.
    const open = message(1, "final", { nodeId: "same-node", messageId: "original" });
    const delta = message(2, " text", { nodeId: "same-node", messageId: "original" });
    expect(assembleTranscript([suffix, open, delta])).toMatchObject([
      { id: "same-node", text: "final text", status: "complete" },
    ]);
  });

  it("does not move a node when late backfill prepends journal history", () => {
    // Tail arrived first (store order), as in gap backfill: events 1..2 are
    // appended to the events array only after events 4..5 were rendered.
    const tailUser = message(4, "question", { nodeId: "n4", messageId: "m4", role: "user" });
    const tailAssistant = message(5, "answer", { nodeId: "n5", messageId: "m5", role: "assistant" });
    const earlyUser = message(1, "first prompt", { nodeId: "n1", messageId: "m1", role: "user" });
    const earlyAssistant = message(2, "first reply", { nodeId: "n2", messageId: "m2", role: "assistant" });
    const withTail = assembleTranscript([tailUser, tailAssistant]);
    const withBackfill = assembleTranscript([tailUser, tailAssistant, earlyUser, earlyAssistant]);
    expect(withTail.map((n) => n.id)).toEqual(["n4", "n5"]);
    // Late-arriving history takes its journal position instead of landing at
    // the bottom, and the tail nodes keep their ids.
    expect(withBackfill.map((n) => n.id)).toEqual(["n1", "n2", "n4", "n5"]);
    // Reassembly in seq order (re-follow) yields exactly the same sequence.
    const seqOrder = assembleTranscript([earlyUser, earlyAssistant, tailUser, tailAssistant]);
    expect(seqOrder.map((n) => n.id)).toEqual(withBackfill.map((n) => n.id));
  });

  it("keeps failed and denied tools inline instead of folding them away", () => {
    const user: TranscriptNode = {
      type: "message", id: "u", role: "user", text: "q", status: "complete", origin: "human",
    };
    const assistant: TranscriptNode = {
      type: "message", id: "a", role: "assistant", text: "ok", status: "complete", origin: "human",
    };
    const mkTool = (idVal: string, outcome: "succeeded" | "failed" | "denied" | null): TranscriptNode => ({
      type: "tool",
      id: idVal,
      family: "Bash",
      name: "Bash",
      driverKind: "claude-print",
      call: call("Bash", idVal),
      result: outcome
        ? {
            nodeId: `r-${idVal}` as Id,
            toolCallId: idVal as Id,
            revision: "1",
            baseRevision: null,
            operation: "close",
            stage: "final",
            outcome,
            blocks: [{ type: "text", text: outcome }],
            structuredResult: unknownKnowledge("text"),
            exitCode: known(1),
            changes: [],
          }
        : null,
      completeness: "structured",
      diffState: "unknown",
    });
    const nodes = [
      user,
      mkTool("ok-1", "succeeded"),
      mkTool("ok-2", "succeeded"),
      mkTool("boom", "failed"),
      mkTool("nope", "denied"),
      assistant,
    ];
    expect(isToolFailure(mkTool("boom", "failed"))).toBe(true);
    expect(isToolFailure(mkTool("ok-1", "succeeded"))).toBe(false);
    const folded = compactTranscript(nodes, true);
    // The two failures stay at top level and never enter the compact group;
    // only successful tools are folded away.
    const top = folded.filter((n) => n.type === "tool");
    expect(top.map((n) => n.id)).toEqual(["boom", "nope"]);
    const group = folded.find((n) => n.type === "compact");
    expect(group?.type === "compact" && group.children.map((c) => c.id)).toEqual(["ok-1", "ok-2"]);
  });

  it("marks a tool failure assembled from observations", () => {
    const nodes = assembleTranscript([
      obs(1, "tool_call", call("Bash", "c-fail")),
      result(2, "c-fail", "failed"),
    ]);
    expect(nodes).toHaveLength(1);
    expect(isToolFailure(nodes[0]!)).toBe(true);
  });
});
