import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { Observation } from "../../../types/observation";
import { known, unknownKnowledge, type Id } from "../../../types/wire";
import { SubagentView } from "./SubagentView";
import * as api from "./subagentApi";

function obs(seq: number, kind: Observation["kind"], payload: unknown, agentId?: string): Observation {
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
    observedAt: "2026-09-16T00:00:00.000Z",
    nativeAt: known("2026-09-16T00:00:00.000Z"),
    source: {
      driverKind: "claude-print",
      driverVersion: "1",
      adapterVersion: "1",
      channel: "transcript",
      delivery: "replay",
      nativeSessionId: known("sub-sess"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: agentId ? known(agentId) : unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" },
      sourceCursor: { type: "runtime", ledgerRevision: String(seq) },
    },
    kind,
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload,
  } as Observation;
}

const promptEvent = obs(1, "message", {
  nodeId: "m_prompt" as Id,
  messageId: "m_prompt" as Id,
  revision: "1",
  operation: "open",
  baseRevision: null,
  role: "user",
  phase: "input",
  blocks: [{ type: "text", text: "audit the auth module" }],
  targetBlock: null,
  parentToolCallId: null,
  nativeOrigin: known("human"),
  origin: "human",
  status: "complete",
});

const toolEvent = obs(2, "tool_call", {
  nodeId: "n1" as Id,
  revision: "1",
  operation: "open",
  baseRevision: null,
  toolCallId: "toolu_grep1" as Id,
  parentToolCallId: null,
  toolName: known("Grep"),
  displayTitle: known("Grep"),
  category: "search",
  input: known({ pattern: "token" }),
  inputTextDelta: null,
  state: "running",
  executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
});

const resultEvent = obs(3, "tool_result", {
  nodeId: "n1" as Id,
  revision: "1",
  operation: "close",
  baseRevision: null,
  toolCallId: "toolu_grep1" as Id,
  stage: "final",
  outcome: "succeeded",
  blocks: [{ type: "text", text: "2 matches" }],
  structuredResult: known({}),
  exitCode: known(0),
  changes: [],
});

const finalEvent = obs(4, "message", {
  nodeId: "m_final" as Id,
  messageId: "m_final" as Id,
  revision: "1",
  operation: "close",
  baseRevision: null,
  role: "assistant",
  phase: "final",
  blocks: [{ type: "text", text: "auth audit complete: 2 findings" }],
  targetBlock: null,
  parentToolCallId: null,
  nativeOrigin: known("assistant"),
  status: "complete",
});

function renderAt(route: string) {
  return render(
    <MemoryRouter initialEntries={[route]}>
      <Routes>
        <Route path="/s/:instanceId/agents/:agentId" element={<SubagentView />} />
        <Route path="/s/:instanceId" element={<div data-testid="parent-session">parent</div>} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("SubagentView drill-in", () => {
  it("renders the subagent transcript through the structured pipeline", async () => {
    const fetchSpy = vi
      .spyOn(api, "fetchSubagentTranscript")
      .mockResolvedValue({
        available: true,
        meta: {
          agentId: "sub123",
          runId: "wf_x",
          prompt: "audit the auth module",
          model: "claude-opus-5",
          tokens: 1234,
          calls: 1,
          latestTool: "Grep",
          startedAt: "2026-09-16T00:00:00.000Z",
          endedAt: "2026-09-16T00:00:42.000Z",
          finalText: "auth audit complete",
        },
        events: [promptEvent, toolEvent, resultEvent, finalEvent],
      });

    renderAt("/s/ins_1/agents/sub123");
    expect(fetchSpy).toHaveBeenCalledWith("ins_1", "sub123");

    await waitFor(() => expect(screen.getByTestId("subagent-view")).toBeTruthy());
    expect(screen.getAllByText("audit the auth module").length).toBeGreaterThan(0);
    expect(screen.getByText("Grep")).toBeTruthy();
    expect(screen.getByText("auth audit complete: 2 findings")).toBeTruthy();
    expect(screen.getByText(/model · claude-opus-5/)).toBeTruthy();
    expect(screen.getByTestId("subagent-back")).toHaveAttribute("href", "/s/ins_1");
  });

  it("shows 启动中 while the transcript has not landed", async () => {
    vi.spyOn(api, "fetchSubagentTranscript").mockResolvedValue({
      available: false,
      events: [],
    });
    renderAt("/s/ins_1/agents/sub999");
    await waitFor(() => expect(screen.getByTestId("subagent-starting")).toBeTruthy());
    expect(screen.queryByTestId("subagent-scroller")).toBeNull();
  });

  it("returns to the parent session on back", async () => {
    vi.spyOn(api, "fetchSubagentTranscript").mockResolvedValue({
      available: true,
      events: [],
    });
    renderAt("/s/ins_1/agents/sub123");
    await waitFor(() => expect(screen.getByTestId("subagent-view")).toBeTruthy());
    await userEvent.setup().click(screen.getByTestId("subagent-back"));
    expect(screen.getByTestId("parent-session")).toBeTruthy();
  });
});
