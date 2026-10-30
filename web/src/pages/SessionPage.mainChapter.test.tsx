import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/observation";
import { known, unknownKnowledge, type Id } from "../types/wire";
import { SessionPage } from "./SessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
  COMPACT_WORKBENCH_QUERY: "(max-width: 767px)",
}));

vi.mock("../features/session/live/LiveStatusStrip", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../features/session/live/LiveStatusStrip")>()),
  LiveStatusStrip: () => <div data-testid="live-status-strip" />,
}));

/** Chapter 1 of a Main lineage: ended and fenced by an owner resume. */
const chapter1 = {
  ...mockDb.instances[0],
  id: "ins_main_g1" as Id,
  lifecycle: "exited" as const,
  activity: { state: "known", value: "idle" } as const,
  activeRunIds: [],
  lineageId: "ins_main_g1" as Id,
  generation: 1,
  fencedAt: "2026-10-30T09:00:00Z",
  grants: ["address-owner"],
} as Instance;

/** Chapter 2: the live current chapter. */
const chapter2 = {
  ...chapter1,
  id: "ins_main_g2" as Id,
  lifecycle: "running" as const,
  connectivity: "connected" as const,
  generation: 2,
  chapterCause: "owner-resume",
  fencedAt: null,
} as Instance;

function renderPage(id: string) {
  return render(
    <MemoryRouter initialEntries={[`/s/${id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "refreshLineage").mockResolvedValue(null);
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [chapter1, chapter2],
    events: { [chapter1.id]: [], [chapter2.id]: [] },
  });
});
afterEach(() => vi.restoreAllMocks());

it("an earlier Main chapter offers no resume, only a link to the current Main", () => {
  const resume = vi.spyOn(store.hubStore, "resume");
  renderPage(chapter1.id);
  expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
  expect(screen.queryByTestId("resume-control")).toBeNull();
  expect(screen.queryByText("继续（结构化）")).toBeNull();
  expect(screen.queryByTestId("resume-terminal")).toBeNull();
  expect(screen.queryByTestId("main-resume")).toBeNull();
  const link = screen.getByTestId("ended-open-current-main");
  expect(link).toHaveTextContent("打开当前 Main");
  expect(link).toHaveAttribute("href", `/s/${chapter2.id}`);
  expect(resume).not.toHaveBeenCalled();
});

it("a plain ended session keeps its resume", () => {
  const plain = { ...chapter1, id: "ins_plain" as Id, lineageId: "ins_plain" as Id, grants: [], fencedAt: null } as Instance;
  vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
    ...store.hubStore.getSnapshot(),
    instances: [plain, chapter2],
    events: { [plain.id]: [], [chapter2.id]: [] },
  });
  renderPage(plain.id);
  expect(screen.getByTestId("resume-control")).toBeInTheDocument();
  expect(screen.queryByTestId("ended-open-current-main")).toBeNull();
});

it("the current Main chapter after Pause has exactly one resume control and reads 已结束 (r3 item 1)", () => {
  // Pause closed the only chapter: it ended and no successor exists. The Hub
  // records no paused-by yet (ma-fence), so the label stays neutral.
  const paused = { ...chapter1, fencedAt: null, updatedAt: "2026-10-30T09:00:00Z" } as Instance;
  vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
    ...store.hubStore.getSnapshot(),
    instances: [paused],
    events: { [paused.id]: [] },
  });
  renderPage(paused.id);
  expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
  expect(screen.getAllByTestId("main-resume")).toHaveLength(1);
  expect(screen.queryByTestId("resume-control")).toBeNull();
  expect(screen.queryByText("继续（结构化）")).toBeNull();
  expect(screen.queryByTestId("resume-terminal")).toBeNull();
  expect(screen.getByTestId("main-status-lineage").textContent).toMatch(/^已结束/);
  expect(screen.getByTestId("main-session-bar").textContent).not.toContain("已停止");
});

/** A complete assistant message observation (the transcript reads it too). */
function reply(instanceId: string, observedAt: string): Observation {
  return {
    schemaVersion: 1,
    eventId: "evt_4" as Id,
    journalId: `obj_${instanceId}` as Id,
    instanceId: instanceId as Id,
    runId: null,
    hostId: "hst" as Id,
    processGeneration: "1",
    runGeneration: null,
    seq: "4",
    observedAt,
    nativeAt: known(observedAt),
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
    },
    kind: "message",
    completeness: "structured",
    rawRef: null,
    evidenceEventIds: [],
    payload: {
      nodeId: "n_4" as Id,
      messageId: "m_4" as Id,
      role: "assistant",
      phase: "final",
      revision: "1",
      baseRevision: null,
      operation: "open",
      blocks: [{ type: "text", text: "好的" }],
      targetBlock: null,
      parentToolCallId: null,
      nativeOrigin: known("assistant"),
      status: "complete",
    },
  } as unknown as Observation;
}

describe("最近活动 (r3 item 2)", () => {
  it("comes from the last message or turn observation, not the row's updatedAt", () => {
    const tenMinutesAgo = new Date(Date.now() - 10 * 60_000).toISOString();
    const working = { ...chapter2, updatedAt: new Date().toISOString() } as Instance;
    vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      instances: [chapter1, working],
      events: {
        [chapter1.id]: [],
        [working.id]: [
          reply(working.id, tenMinutesAgo),
        ],
      },
    });
    renderPage(working.id);
    expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 10m");
  });

  it("reads — while no activity is known", () => {
    const working = { ...chapter2, updatedAt: new Date().toISOString() } as Instance;
    vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      instances: [chapter1, working],
      events: { [chapter1.id]: [], [working.id]: [] },
    });
    renderPage(working.id);
    expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 —");
  });
});
