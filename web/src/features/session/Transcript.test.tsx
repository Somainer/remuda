import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { buildLongObservations } from "../../fixtures/session/longEvents";
import type { Observation } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import { Transcript } from "./Transcript";

function obs(seq: number, kind: Observation["kind"], payload: unknown, instanceId = "ins_t"): Observation {
  return {
    schemaVersion: 1,
    eventId: `evt_${seq}` as Id,
    journalId: "obj_t" as Id,
    instanceId: instanceId as Id,
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

function userMessage(seq: number, text: string): Observation {
  return obs(seq, "message", {
    nodeId: `n-${seq}` as Id, messageId: `m-${seq}` as Id, role: "user", phase: "input",
    revision: "1", baseRevision: null, operation: "open",
    blocks: [{ type: "text", text }], targetBlock: null, parentToolCallId: null,
    nativeOrigin: known("ui"), status: "complete",
  });
}

function assistantMessage(seq: number, text: string): Observation {
  return obs(seq, "message", {
    nodeId: `n-${seq}` as Id, messageId: `m-${seq}` as Id, role: "assistant", phase: "final",
    revision: "1", baseRevision: null, operation: "open",
    blocks: [{ type: "text", text }], targetBlock: null, parentToolCallId: null,
    nativeOrigin: known("assistant"), status: "complete",
  });
}

function failedTool(seqCall: number, seqResult: number): Observation[] {
  return [
    obs(seqCall, "tool_call", {
      nodeId: `nc-${seqCall}` as Id, revision: "1", operation: "open", baseRevision: null,
      toolCallId: `tc-fail` as Id, parentToolCallId: null,
      toolName: known("Bash"), displayTitle: known("Bash"), category: "shell",
      input: known({ command: "make test" }), inputTextDelta: null, state: "running",
      executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
    }),
    obs(seqResult, "tool_result", {
      nodeId: `nr-${seqResult}` as Id, revision: "1", operation: "close", baseRevision: null,
      toolCallId: "tc-fail" as Id, stage: "final", outcome: "failed",
      blocks: [{ type: "text", text: "tests failed" }], structuredResult: unknownKnowledge("text"),
      exitCode: known(1), changes: [],
    }),
  ];
}

function settledBash(seqCall: number, seqResult: number): Observation[] {
  return [
    obs(seqCall, "tool_call", {
      nodeId: `nc-${seqCall}` as Id, revision: "1", operation: "open", baseRevision: null,
      toolCallId: "tc-bash" as Id, parentToolCallId: null,
      toolName: known("Bash"), displayTitle: known("Bash"), category: "shell",
      input: known({ command: "echo silent-command" }), inputTextDelta: null, state: "running",
      executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
    }),
    obs(seqResult, "tool_result", {
      nodeId: `nr-${seqResult}` as Id, revision: "1", operation: "close", baseRevision: null,
      toolCallId: "tc-bash" as Id, stage: "final", outcome: "succeeded",
      blocks: [{ type: "text", text: "zorpto-searchfind-4711" }],
      structuredResult: unknownKnowledge("text"),
      exitCode: known(0), changes: [],
    }),
  ];
}

/** Match the workbench compact query (a 390px layout); anything else = desktop. */
function stubCompactLayout() {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query.includes("max-width: 767px"),
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }));
}

function renderRouted(events: Observation[], path = "/s/ins_t", compact = true) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/s/:instanceId" element={<Transcript events={events} compact={compact} />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("Transcript", () => {
  it("fills an empty completed message from delayed history without remounting its bubble", () => {
    const base = buildLongObservations({ instanceId: "ins_stream", journalId: "obj_stream", hostId: "hst_1", count: 1 })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = { ...base, payload: { ...base.payload, status: "streaming" as const, blocks: [{ type: "text" as const, text: "hello from web hub" }] } };
    const close = {
      ...base, eventId: "evt_close", seq: "2",
      payload: { ...base.payload, messageId: "renamed", operation: "close" as const, revision: "2", baseRevision: "1", blocks: [] },
    };
    const { rerender } = render(<Transcript events={[close]} compact={false} />);
    const bubble = screen.getByTestId("message");
    expect(bubble).toHaveTextContent("You");
    rerender(<Transcript events={[close, open]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("hello from web hub");
  });

  it("updates the same assistant bubble while stream revisions arrive", () => {
    const base = buildLongObservations({ instanceId: "ins_stream", journalId: "obj_stream", hostId: "hst_1", count: 1 })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = { ...base, payload: { ...base.payload, role: "assistant" as const, status: "streaming" as const, blocks: [{ type: "text" as const, text: "我" }] } };
    const append = {
      ...open, eventId: "evt_append", seq: "2",
      payload: { ...open.payload, operation: "append" as const, revision: "2", baseRevision: "1", targetBlock: 0, blocks: [{ type: "text" as const, text: "先看看" }] },
    };
    const close = {
      ...open, eventId: "evt_close", seq: "3",
      payload: { ...open.payload, operation: "close" as const, revision: "3", baseRevision: "2", status: "complete" as const, blocks: [{ type: "text" as const, text: "我先看看" }] },
    };
    const { rerender } = render(<Transcript events={[open]} compact={false} />);
    const bubble = screen.getByTestId("message");
    expect(bubble).toHaveTextContent("我");
    rerender(<Transcript events={[open, append]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("我先看看");
    rerender(<Transcript events={[open, append, close]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("我先看看");
  });

  it("virtualizes a 2000-event fixture", () => {
    const events = buildLongObservations({
      instanceId: "ins_long" as Id,
      journalId: "obj_long" as Id,
      hostId: "hst_1" as Id,
      count: 2000,
    });
    render(<Transcript events={events} compact={false} />);
    expect(screen.getByTestId("transcript")).toBeTruthy();
    expect(screen.getByTestId("transcript-scroller")).toBeTruthy();
    expect(screen.getAllByTestId("transcript-row").length).toBeLessThan(80);
    expect(screen.getAllByTestId("transcript-row").length).toBeGreaterThan(0);
  });

  it("moves the active turn with j/k", async () => {
    const user = userEvent.setup();
    const events = buildLongObservations({
      instanceId: "ins_long" as Id,
      journalId: "obj_long" as Id,
      hostId: "hst_1" as Id,
      count: 8,
    });
    const { container } = render(<Transcript events={events} compact={false} />);
    await user.keyboard("j");
    expect(container.querySelector('[data-turn-active="1"]')).toBeTruthy();
    await user.keyboard("k");
    expect(container.querySelector("[data-anchor]")).toBeTruthy();
  });

  it("does not rebuild row ResizeObservers on parent scroll re-renders", () => {
    const ROW = 96;
    const VIEW = 720;
    const isScroller = (el: unknown) =>
      el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? VIEW : 0;
    });
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? 200 * ROW : 0;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
      height: ROW - 12, top: 0, left: 0, right: 0, bottom: 0, width: 0, x: 0, y: 0, toJSON() {},
    } as DOMRect);

    let observersCreated = 0;
    const observersDisconnected = vi.fn();
    class CountingResizeObserver {
      constructor() {
        observersCreated += 1;
      }
      observe() {}
      unobserve() {}
      disconnect() {
        observersDisconnected();
      }
    }
    vi.stubGlobal("ResizeObserver", CountingResizeObserver);

    const events = buildLongObservations({
      instanceId: "ins_ro" as Id,
      journalId: "obj_ro" as Id,
      hostId: "hst_1" as Id,
      count: 200,
    });
    const { unmount } = render(<Transcript events={events} compact={false} />);
    const mountedRows = screen.getAllByTestId("transcript-row").length;
    expect(mountedRows).toBeGreaterThan(0);
    // StrictMode replays effects on mount, so observer count need not equal the
    // row count; record the post-mount steady state instead.
    const afterMount = observersCreated;

    // Every scroll frame re-renders the parent (setScrollTop). Keep the deltas
    // inside the first 96px row so the virtual window mounts exactly the same
    // rows — any observer churn here is purely the unstable-callback defect,
    // not virtualization mounting newly-visible rows.
    const scroller = screen.getByTestId("transcript-scroller") as HTMLElement;
    for (const top of [1, 2, 3, 4, 5]) {
      (scroller as HTMLElement & { scrollTop: number }).scrollTop = top;
      fireEvent.scroll(scroller);
    }
    expect(observersCreated).toBe(afterMount);
    expect(observersDisconnected).not.toHaveBeenCalled();

    unmount();
    expect(observersDisconnected.mock.calls.length).toBeGreaterThan(0);
    vi.unstubAllGlobals();
  });
});

describe("Transcript search (batch E)", () => {
  // jsdom performs no layout, so virtualization math gets zeros unless rows
  // get a deterministic height (84 + the component's 12px margin = 96,
  // matching virtualWindow.DEFAULT_ROW) and the scroller a viewport/height.
  // Patches live on the prototypes so they also cover a remounted scroller.
  afterEach(() => {
    vi.restoreAllMocks();
  });

  function stubLayout(rowCount: number) {
    const ROW = 96;
    const VIEW = 720;
    const isScroller = (el: unknown) =>
      el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? VIEW : 0;
    });
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? rowCount * ROW : 0;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
      height: ROW - 12, top: 0, left: 0, right: 0, bottom: 0, width: 0, x: 0, y: 0, toJSON() {},
    } as DOMRect);
  }

  it("finds a loaded node outside the virtual window and scrolls/highlights it", async () => {
    const user = userEvent.setup();
    const events = buildLongObservations({
      instanceId: "ins_long" as Id,
      journalId: "obj_long" as Id,
      hostId: "hst_1" as Id,
      count: 2000,
    });
    stubLayout(2000);
    render(<Transcript events={events} compact={false} />);
    const scroller = screen.getByTestId("transcript-scroller") as HTMLElement;

    await user.click(screen.getByTestId("transcript-search-open"));
    const input = screen.getByTestId("transcript-search-input");
    await user.type(input, "prompt 1357");
    expect(screen.getByTestId("transcript-search-count").textContent).toMatch(/1\/1/);

    await user.keyboard("[Enter]");
    // The hit is node index 1356, far below the ~24 rendered rows; the
    // scroller must have been positioned there from assembled geometry, never
    // by querying a DOM node the virtual window did not mount.
    expect((scroller as HTMLElement & { scrollTop: number }).scrollTop).toBe(1356 * 96);
    // jsdom does not fire `scroll` on programmatic scrollTop writes; do what
    // the browser does so the windowing state catches up.
    fireEvent.scroll(scroller);
    const current = document.querySelector('[data-search-current="1"]');
    expect(current?.getAttribute("data-anchor")).toBe("obj_long_n_1357");
  });

  it("restores the reading position after leaving and re-entering the session", () => {
    localStorage.clear();
    const events = buildLongObservations({
      instanceId: "ins_restore" as Id,
      journalId: "obj_restore" as Id,
      hostId: "hst_1" as Id,
      count: 60,
    });
    stubLayout(60);
    const first = renderRouted(events, "/s/ins_restore", false);
    const scroller = screen.getByTestId("transcript-scroller") as HTMLElement;
    // Read something in the middle, then navigate away (unmount flushes the
    // debounced save synchronously).
    (scroller as HTMLElement & { scrollTop: number }).scrollTop = 2400;
    fireEvent.scroll(scroller);
    first.unmount();
    expect(localStorage.getItem("runtime.reading.v1.ins_restore")).toContain("\"follow\":false");

    renderRouted(events, "/s/ins_restore", false);
    const scroller2 = screen.getByTestId("transcript-scroller") as HTMLElement;
    expect((scroller2 as HTMLElement & { scrollTop: number }).scrollTop).toBe(2400);
    localStorage.clear();
  });

  it("re-pins follow sessions to the latest on re-entry", () => {
    localStorage.clear();
    const events = buildLongObservations({
      instanceId: "ins_follow" as Id,
      journalId: "obj_follow" as Id,
      hostId: "hst_1" as Id,
      count: 60,
    });
    stubLayout(60);
    const first = renderRouted(events, "/s/ins_follow", false);
    first.unmount();
    const saved = JSON.parse(localStorage.getItem("runtime.reading.v1.ins_follow") ?? "{}");
    expect(saved.follow).toBe(true);
    const second = renderRouted(events, "/s/ins_follow", false);
    // A following transcript never restores an old offset and re-pins to end.
    const scroller2 = screen.getByTestId("transcript-scroller") as HTMLElement;
    expect((scroller2 as HTMLElement & { scrollTop: number }).scrollTop).toBe(60 * 96);
    second.unmount();
    localStorage.clear();
  });

  it("keeps a failed tool visible inline and immune to collapse-all", async () => {
    const user = userEvent.setup();
    // User prompt, one failed tool, then the assistant turn ends: compaction
    // would normally fold the tool into the process group.
    const events: Observation[] = [
      userMessage(1, "跑一下测试"),
      ...failedTool(2, 3),
      assistantMessage(4, "有测试挂了"),
    ];
    renderRouted(events, "/s/ins_fail");
    expect(screen.getByTestId("tool-failure-tag").textContent).toContain("失败");
    const card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("0");
    // Collapse-all folds routine cards; a failure must not disappear.
    await user.click(screen.getByTestId("collapse-all"));
    expect(screen.getByTestId("tool-failure-tag")).toBeTruthy();
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
  });

  it("keeps aria-live off on the transcript root while search is open", async () => {
    const user = userEvent.setup();
    renderRouted([userMessage(1, "alpha"), assistantMessage(2, "beta")], "/s/ins_live");
    const root = screen.getByTestId("transcript");
    expect(root.getAttribute("aria-live")).toBe("off");
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "beta");
    expect(root.getAttribute("aria-live")).toBe("off");
    expect(screen.getByTestId("transcript-search-count").getAttribute("aria-live")).toBe("off");
  });
});

describe("D-041 fold vs in-transcript search hit", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("auto-expands a folded card that becomes the current search hit (and re-folds after)", async () => {
    stubCompactLayout();
    const user = userEvent.setup();
    // One turn; the settled Bash and the assistant message do NOT reach the
    // >=2 compact-fold threshold, so the card is a direct top-level row.
    renderRouted(
      [
        userMessage(1, "跑一下"),
        ...settledBash(2, 3),
        assistantMessage(4, "done"),
      ],
      "/s/ins_hit",
    );
    const card = screen.getByTestId("tool-card");
    // Compact + settled ordinary card starts folded; result text is hidden.
    expect(card.getAttribute("data-folded")).toBe("1");
    expect(screen.queryByText("zorpto-searchfind-4711")).toBeNull();

    // Search for text that exists ONLY in the tool result.
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "zorpto-searchfind-4711");

    // The hit auto-expands the D-041 fold so the match is visible and the
    // full card body (stdout) is mounted.
    await expect
      .poll(() => screen.getByTestId("tool-card").getAttribute("data-folded"))
      .toBe("0");
    expect(screen.getByText("zorpto-searchfind-4711")).toBeTruthy();

    // Clearing the search removes the transient hit expansion; the card is
    // its compact default again (the reader did not press 展开).
    await user.clear(screen.getByTestId("transcript-search-input"));
    await expect
      .poll(() => screen.getByTestId("tool-card").getAttribute("data-folded"))
      .toBe("1");
  });
});
