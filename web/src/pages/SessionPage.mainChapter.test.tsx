import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
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

describe("the EndedBar note only points at a 恢复 the Main bar offers (r4 item 1)", () => {
  function only(rows: Instance[], lineages: Record<string, unknown> = {}) {
    vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      instances: rows,
      lineages: lineages as never,
      events: Object.fromEntries(rows.map((row) => [row.id, []])),
    });
  }

  it("host contact lost: the chapter may still run, the bar offers no 恢复, so no note and the bar's own resume path stays", () => {
    const lost = { ...chapter1, fencedAt: null, lastError: "host-contact-lost" } as Instance;
    only([lost]);
    renderPage(lost.id);
    expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
    expect(screen.queryByTestId("main-resume")).toBeNull();
    expect(screen.queryByTestId("ended-resume-in-main-bar")).toBeNull();
    expect(screen.getByTestId("resume-control")).toBeInTheDocument();
  });

  it("a successor the list has not shown yet: the bar reads 正在启动下一章 with no 恢复, so no note", () => {
    const ended = { ...chapter1, fencedAt: null } as Instance;
    only([ended], {
      [ended.id]: {
        lineageId: ended.id,
        state: "starting",
        generation: 2,
        chapters: [
          { instanceId: ended.id, generation: 1, createdAt: "2026-10-30T08:00:00Z" },
          { instanceId: "ins_main_next", generation: 2, createdAt: "2026-10-30T09:00:00Z" },
        ],
      },
    });
    renderPage(ended.id);
    expect(screen.queryByTestId("main-resume")).toBeNull();
    expect(screen.queryByTestId("ended-resume-in-main-bar")).toBeNull();
    expect(screen.getByTestId("resume-control")).toBeInTheDocument();
  });

  it("no resumable transcript: no note, and the 开新会话继承 cwd fallback is reachable", () => {
    const noResume = {
      ...chapter1,
      fencedAt: null,
      capabilities: {
        ...chapter1.capabilities,
        capabilities: { ...chapter1.capabilities.capabilities, resume: { state: "unsupported" } },
      },
    } as Instance;
    only([noResume]);
    renderPage(noResume.id);
    expect(screen.queryByTestId("ended-resume-in-main-bar")).toBeNull();
    expect(screen.getByText("开新会话继承 cwd")).toBeInTheDocument();
  });
});

describe("最近活动 on an earlier chapter's page reads the bounded tail (r4 item 2)", () => {
  const minutesAgo = (n: number) => new Date(Date.now() - n * 60_000).toISOString();
  const g1 = { ...chapter1, journalId: "obj_main_g1" as Id } as Instance;
  const g2 = { ...chapter2, journalId: "obj_main_g2" as Id, durableSeq: "4", updatedAt: new Date().toISOString() } as Instance;

  function tailWith(observedAt: string) {
    return vi.spyOn(api, "eventsRead").mockImplementation(async ({ journalId }) => ({
      events: journalId === g2.journalId ? [reply(g2.id, observedAt)] : [],
      durableSeq: "4" as never,
      windowFromSeq: "4" as never,
      reachedAfterSeq: true,
    }));
  }

  it("after visiting the current chapter, the earlier chapter's page does not freeze on its events", async () => {
    // Visiting chapter 2 left its followed journal in the store (never
    // pruned): its last message is 10 minutes old.
    vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      instances: [g1, g2],
      events: { [g1.id]: [], [g2.id]: [reply(g2.id, minutesAgo(10))] },
    });
    const current = renderPage(g2.id);
    expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 10m");
    current.unmount();
    // Main kept working: its journal now has a message from 1 minute ago.
    const read = tailWith(minutesAgo(1));
    renderPage(g1.id);
    await waitFor(() => expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 1m"));
    expect(read).toHaveBeenCalledWith(expect.objectContaining({ journalId: g2.journalId }));
  });

  it("the tail path: — until the current chapter's tail answers, then its last activity", async () => {
    vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      instances: [g1, g2],
      events: { [g1.id]: [] },
    });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    vi.spyOn(api, "eventsRead").mockImplementation(async ({ journalId }) => {
      await gate;
      return {
        events: journalId === g2.journalId ? [reply(g2.id, minutesAgo(3))] : [],
        durableSeq: "4" as never,
        windowFromSeq: "4" as never,
        reachedAfterSeq: true,
      };
    });
    renderPage(g1.id);
    expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 —");
    release();
    await waitFor(() => expect(screen.getByTestId("main-status-last-activity").textContent).toBe("最近活动 3m"));
  });
});
