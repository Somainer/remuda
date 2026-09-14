/**
 * Message authorship and streaming in the 结构 view (D-028 P3).
 *
 * Two user reports drive this file:
 *
 * - «现在 Structural 的界面不是按文本流式出现的…我觉得它的实时性不够» — text
 *   must appear as it arrives, with a visible cursor while it is still coming.
 * - «结构化界面会把追加的 prompt 信息也额外展示了 … 1 是有重复，2 是容易让人误解
 *   是我发了这些信息» — only the human's own words may render as a "You" bubble.
 */
import { describe, expect, it, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { MessagePayload, Observation } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import { assembleTranscript } from "./assemble";
import { Transcript } from "./Transcript";

function obs(seq: number, payload: Partial<MessagePayload>): Observation {
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
    observedAt: "2026-09-14T00:00:00.000Z",
    nativeAt: known("2026-09-14T00:00:00.000Z"),
    source: {
      driverKind: "shell-pty",
      driverVersion: "1",
      adapterVersion: "1",
      channel: "hook",
      delivery: "live",
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: "1" },
    },
    kind: "message",
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload: {
      nodeId: `node-${seq}`,
      messageId: `msg-${seq}`,
      role: "user",
      phase: "input",
      revision: "1",
      baseRevision: null,
      operation: "open",
      status: "complete",
      blocks: [{ type: "text", text: "" }],
      targetBlock: 0,
      parentToolCallId: null,
      nativeOrigin: known("n"),
      ...payload,
    },
  } as Observation;
}

/** A user-role message with a given origin, as the mapper would emit it. */
function user(seq: number, text: string, origin: MessagePayload["origin"]): Observation {
  return obs(seq, { role: "user", origin, blocks: [{ type: "text", text }] });
}

/** One chunk of a streaming assistant message, as the hook fold emits it. */
function chunk(seq: number, text: string, opts: { first?: boolean; final?: boolean } = {}): Observation {
  return obs(seq, {
    nodeId: "stream-node",
    messageId: "stream",
    role: "assistant",
    phase: "final",
    revision: String(seq),
    baseRevision: opts.first ? null : String(seq - 1),
    operation: opts.first ? "open" : "append",
    status: opts.final ? "complete" : "streaming",
    blocks: [{ type: "text", text }],
    targetBlock: 0,
  });
}

describe("assembleTranscript · authorship", () => {
  it("carries each message's origin through", () => {
    const nodes = assembleTranscript([
      user(1, "run the tests", "human"),
      user(2, "<command-name>/p3probe</command-name>", "injected-skill"),
      user(3, "<task-notification>done</task-notification>", "hook-context"),
    ]);
    expect(nodes.map((n) => n.type === "message" && n.origin)).toEqual([
      "human",
      "injected-skill",
      "hook-context",
    ]);
  });

  it("reads a message with no origin as the human's own", () => {
    // The field is additive; a pre-D-028 producer omits it, and hiding those
    // messages would silently swallow real conversation.
    const nodes = assembleTranscript([obs(1, { role: "user", blocks: [{ type: "text", text: "hi" }] })]);
    expect(nodes[0]).toMatchObject({ type: "message", origin: "human", text: "hi" });
  });
});

describe("assembleTranscript · incremental streaming", () => {
  it("merges appended chunks into one message rather than re-rendering each", () => {
    const nodes = assembleTranscript([
      chunk(1, "1\n2\n3\n4\n", { first: true }),
      chunk(2, "5\n6\n7\n8", { final: true }),
    ]);
    expect(nodes).toHaveLength(1);
    expect(nodes[0]).toMatchObject({ type: "message", text: "1\n2\n3\n4\n5\n6\n7\n8", status: "complete" });
  });

  it("shows the partial text while the message is still streaming", () => {
    // The whole point: half a message on screen beats an empty pane.
    const nodes = assembleTranscript([chunk(1, "1\n2\n3\n4\n", { first: true })]);
    expect(nodes[0]).toMatchObject({ text: "1\n2\n3\n4\n", status: "streaming" });
  });

  it("grows the same node as each chunk lands", () => {
    const first = chunk(1, "part one ", { first: true });
    const second = chunk(2, "part two", { final: true });
    const afterFirst = assembleTranscript([first]);
    const afterSecond = assembleTranscript([first, second]);
    expect(afterFirst[0].id).toBe(afterSecond[0].id);
    expect(afterSecond[0]).toMatchObject({ text: "part one part two" });
  });
});

describe("Transcript · injected records", () => {
  beforeEach(() => localStorage.clear());

  it("renders only the human's words as a You bubble", () => {
    render(
      <Transcript
        compact={false}
        events={[
          user(1, "run the tests", "human"),
          user(2, "# Workflow authoring reference", "injected-skill"),
        ]}
      />,
    );
    const bubbles = screen.getAllByTestId("message");
    expect(bubbles).toHaveLength(1);
    expect(bubbles[0].textContent).toContain("run the tests");
  });

  it("collapses an injection to a muted row naming its kind and size", () => {
    render(
      <Transcript
        compact={false}
        events={[user(1, "<task-notification>done</task-notification>", "hook-context")]}
      />,
    );
    const row = screen.getByTestId("injected-row");
    expect(row.getAttribute("data-origin")).toBe("hook-context");
    expect(row.textContent).toContain("系统注入");
    expect(row.textContent).toContain("hook 上下文");
    // Expandable, so the text is still readable — it explains the agent's reply.
    expect(row.textContent).toContain("<task-notification>done</task-notification>");
  });

  it("hides injections behind a toggle that persists", async () => {
    const events = [user(1, "hello", "human"), user(2, "injected", "injected-skill")];
    const { unmount } = render(<Transcript compact={false} events={events} />);
    // Collapsed but present by default.
    expect(screen.queryByTestId("injected-row")).not.toBeNull();

    // Hiding them entirely is the opt-in; they explain the agent's replies.
    await userEvent.click(screen.getByTestId("toggle-injected"));
    expect(screen.queryByTestId("injected-row")).toBeNull();
    unmount();

    // The preference survives a remount, so a reader who turned them off does
    // not have to do it again every session.
    render(<Transcript compact={false} events={events} />);
    expect(screen.queryByTestId("injected-row")).toBeNull();
    expect(screen.getByTestId("toggle-injected").textContent).toContain("显示注入内容");
  });

  it("offers no toggle when there is nothing injected to hide", () => {
    render(<Transcript compact={false} events={[user(1, "hello", "human")]} />);
    expect(screen.queryByTestId("toggle-injected")).toBeNull();
  });

  it("never hides an assistant message, whatever its origin", () => {
    render(
      <Transcript
        compact={false}
        events={[obs(1, { role: "assistant", origin: "tool-result", blocks: [{ type: "text", text: "reply" }] })]}
      />,
    );
    expect(screen.getByTestId("message").textContent).toContain("reply");
  });
});

describe("Transcript · streaming cursor", () => {
  beforeEach(() => localStorage.clear());

  it("shows a cursor while text is still arriving and drops it when complete", () => {
    const first = chunk(1, "half a thought", { first: true });
    const { rerender } = render(<Transcript compact={false} events={[first]} />);
    expect(screen.queryByTestId("streaming-cursor")).not.toBeNull();

    rerender(<Transcript compact={false} events={[first, chunk(2, " completed", { final: true })]} />);
    expect(screen.queryByTestId("streaming-cursor")).toBeNull();
    expect(screen.getByTestId("message").textContent).toContain("half a thought completed");
  });

  it("labels a queued message as queued and an interrupted one as interrupted", () => {
    render(
      <Transcript
        compact={false}
        events={[
          obs(1, { role: "user", origin: "human", status: "queued", blocks: [{ type: "text", text: "later" }] }),
          obs(2, {
            nodeId: "n2",
            messageId: "m2",
            role: "user",
            origin: "human",
            status: "interrupted",
            blocks: [{ type: "text", text: "never sent" }],
          }),
        ]}
      />,
    );
    const bubbles = screen.getAllByTestId("message");
    expect(bubbles[0].textContent).toContain("排队中");
    expect(bubbles[1].textContent).toContain("已打断");
  });
});
