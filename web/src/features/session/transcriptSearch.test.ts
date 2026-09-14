import { describe, expect, it } from "vitest";
import type { Id } from "../../types/wire";
import { known, unknownKnowledge } from "../../types/wire";
import type { TranscriptNode } from "./assemble";
import { findMatches, resolveSelection, searchableText } from "./transcriptSearch";

function message(id: string, role: "user" | "assistant", text: string): TranscriptNode {
  return { type: "message", id, role, text, status: "complete", origin: role === "user" ? "human" : "human" };
}

function bashTool(idVal: string, command: string, outcome?: "succeeded" | "failed" | "denied", output = ""): TranscriptNode {
  return {
    type: "tool",
    id: idVal,
    family: "Bash",
    name: "Bash",
    driverKind: "claude-print",
    call: {
      nodeId: `n-${idVal}` as Id,
      revision: "5",
      operation: "close",
      baseRevision: "4",
      toolCallId: idVal as Id,
      parentToolCallId: null,
      toolName: known("Bash"),
      displayTitle: known("Bash"),
      category: "shell",
      input: known({ command }),
      inputTextDelta: null,
      state: "running",
      executor: known({ hostId: "h" as Id, workspaceId: null, nativeAgentId: null }),
    },
    result:
      outcome === undefined
        ? null
        : {
            nodeId: `r-${idVal}` as Id,
            revision: "1",
            operation: "close",
            baseRevision: null,
            toolCallId: idVal as Id,
            stage: "final",
            outcome,
            blocks: output ? [{ type: "text" as const, text: output }] : [],
            structuredResult: unknownKnowledge("text"),
            exitCode: known(outcome === "succeeded" ? 0 : 1),
            changes: [],
          },
    completeness: "structured",
    diffState: "unknown",
  };
}

describe("findMatches", () => {
  it("returns nothing for an empty or whitespace query", () => {
    const nodes = [message("m1", "assistant", "alpha beta")];
    expect(findMatches(nodes, "")).toEqual([]);
    expect(findMatches(nodes, "   ")).toEqual([]);
  });

  it("matches message text case-insensitively with node index and ordinals", () => {
    const nodes = [
      message("m1", "user", "Needle once"),
      message("m2", "assistant", "needle twice: needle"),
      bashTool("t1", "echo needle"),
    ];
    const matches = findMatches(nodes, "NEEDLE");
    expect(matches.map((m) => [m.nodeId, m.ordinal])).toEqual([
      ["m1", 0],
      ["m2", 0],
      ["m2", 1],
      ["t1", 0],
    ]);
    expect(matches.map((m) => m.index)).toEqual([0, 1, 1, 2]);
  });

  it("respects case-sensitive option", () => {
    const nodes = [message("m1", "assistant", "Needle needle")];
    expect(findMatches(nodes, "Needle", { caseSensitive: true })).toHaveLength(1);
    expect(findMatches(nodes, "needle", { caseSensitive: true })).toHaveLength(1);
  });

  it("finds hits in tool inputs and results", () => {
    const nodes = [bashTool("t1", "rg SEARCHME", "succeeded", "SEARCHME found in src/a.ts")];
    const hits = findMatches(nodes, "searchme");
    // One in the input JSON, one in the result text.
    expect(hits.length).toBeGreaterThanOrEqual(2);
    expect(hits.every((m) => m.nodeId === "t1")).toBe(true);
  });

  it("searches folded compact children but keeps the child nodeId", () => {
    const fold: TranscriptNode = {
      type: "compact",
      id: "compact:c1",
      toolCount: 1,
      thoughtCount: 1,
      children: [bashTool("inside", "grep buried"), {
        type: "thought", id: "th1", text: "a buried thought", completeness: "structured",
      }],
    };
    expect(findMatches([fold], "buried").map((m) => m.nodeId)).toEqual(["inside", "th1"]);
    // Matches carry the fold's top-level index so scrolling lands on it.
    expect(findMatches([fold], "buried").map((m) => m.index)).toEqual([0, 0]);
    // A word unique to the thought resolves to the thought child, not the fold.
    expect(findMatches([fold], "thought").map((m) => m.nodeId)).toEqual(["th1"]);
  });

  it("does not match usage-only nodes", () => {
    expect(searchableText({ type: "usage", id: "u", payload: {} as never })).toBeNull();
    expect(findMatches([{ type: "usage", id: "u", payload: {} as never }], "anything")).toEqual([]);
  });

  it("does not throw on odd node shapes", () => {
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    const node = bashTool("t", "");
    if (node.type === "tool") node.call.input = known(cyclic);
    expect(() => findMatches([node], "bash")).not.toThrow();
  });
});

describe("resolveSelection", () => {
  it("starts at the first match", () => {
    const matches = findMatches([message("m1", "assistant", "x x"), message("m2", "assistant", "x")], "x");
    expect(resolveSelection(matches, null)).toBe(0);
  });

  it("returns -1 with no matches", () => {
    expect(resolveSelection([], null)).toBe(-1);
  });

  it("keeps the same (nodeId, ordinal) hit when nodes shift above it", () => {
    const before = [
      message("m1", "assistant", "hit hit"),
      message("m2", "assistant", "hit"),
    ];
    const after = [
      message("m0", "assistant", "hit"),
      message("m1", "assistant", "hit hit"),
      message("m2", "assistant", "hit"),
    ];
    const beforeMatches = findMatches(before, "hit");
    const afterMatches = findMatches(after, "hit");
    // User had selected m2's only hit: index 2 in the old list.
    const selected = beforeMatches[2]!;
    expect(selected.nodeId).toBe("m2");
    const next = resolveSelection(afterMatches, selected);
    // m2's hit now sits at index 3; an index-based selection would land on
    // m1's second hit instead.
    expect(afterMatches[next].nodeId).toBe("m2");
    expect(next).toBe(3);
  });

  it("clamps to the node's remaining hit when an ordinal disappears", () => {
    const previous = findMatches([message("m1", "assistant", "a a a")], "a")[2]!;
    // The tail got replaced; only two occurrences remain, node id is stable.
    const nextMatches = findMatches([message("m1", "assistant", "a a"), message("m2", "assistant", "z")], "a");
    const resolved = resolveSelection(nextMatches, previous);
    expect(nextMatches[resolved].nodeId).toBe("m1");
    expect(nextMatches[resolved].ordinal).toBe(1);
  });

  it("falls back to list position only when the whole node disappeared", () => {
    const previous = findMatches([message("gone", "assistant", "needle")], "needle")[0]!;
    const nextMatches = findMatches(
      [message("a", "assistant", "needle"), message("b", "assistant", "needle")],
      "needle",
    );
    const resolved = resolveSelection(nextMatches, previous);
    expect(nextMatches[resolved].nodeId).toBe("a");
  });
});
