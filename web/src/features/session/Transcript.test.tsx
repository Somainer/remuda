import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act, createRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { buildLongObservations } from "../../fixtures/session/longEvents";
import type { Observation } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import { Transcript, type TranscriptHandle } from "./Transcript";

// The commit probe is only mounted under ?profile=1; the flag is a getter so
// one describe block can turn it on without affecting the rest of the file.
const profile = vi.hoisted(() => ({
  on: false,
  probes: [] as Array<{ kind: string; value: unknown }>,
}));
vi.mock("../../lib/profileFlags", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/profileFlags")>();
  return {
    ...actual,
    get profilingEnabled() {
      return profile.on;
    },
    reportProbe: (kind: string, value: unknown) => {
      profile.probes.push({ kind, value });
    },
  };
});

function obs(
  seq: number,
  kind: Observation["kind"],
  payload: unknown,
  instanceId = "ins_t",
): Observation {
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
    nodeId: `n-${seq}` as Id,
    messageId: `m-${seq}` as Id,
    role: "user",
    phase: "input",
    revision: "1",
    baseRevision: null,
    operation: "open",
    blocks: [{ type: "text", text }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("ui"),
    status: "complete",
  });
}

function assistantMessage(seq: number, text: string): Observation {
  return obs(seq, "message", {
    nodeId: `n-${seq}` as Id,
    messageId: `m-${seq}` as Id,
    role: "assistant",
    phase: "final",
    revision: "1",
    baseRevision: null,
    operation: "open",
    blocks: [{ type: "text", text }],
    targetBlock: null,
    parentToolCallId: null,
    nativeOrigin: known("assistant"),
    status: "complete",
  });
}

function failedTool(seqCall: number, seqResult: number): Observation[] {
  return [
    obs(seqCall, "tool_call", {
      nodeId: `nc-${seqCall}` as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      toolCallId: `tc-fail` as Id,
      parentToolCallId: null,
      toolName: known("Bash"),
      displayTitle: known("Bash"),
      category: "shell",
      input: known({ command: "make test" }),
      inputTextDelta: null,
      state: "running",
      executor: known({
        hostId: "hst" as Id,
        workspaceId: null,
        nativeAgentId: null,
      }),
    }),
    obs(seqResult, "tool_result", {
      nodeId: `nr-${seqResult}` as Id,
      revision: "1",
      operation: "close",
      baseRevision: null,
      toolCallId: "tc-fail" as Id,
      stage: "final",
      outcome: "failed",
      blocks: [{ type: "text", text: "tests failed" }],
      structuredResult: unknownKnowledge("text"),
      exitCode: known(1),
      changes: [],
    }),
  ];
}

function settledBash(seqCall: number, seqResult: number): Observation[] {
  return [
    obs(seqCall, "tool_call", {
      nodeId: `nc-${seqCall}` as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      toolCallId: "tc-bash" as Id,
      parentToolCallId: null,
      toolName: known("Bash"),
      displayTitle: known("Bash"),
      category: "shell",
      input: known({ command: "echo silent-command" }),
      inputTextDelta: null,
      state: "running",
      executor: known({
        hostId: "hst" as Id,
        workspaceId: null,
        nativeAgentId: null,
      }),
    }),
    obs(seqResult, "tool_result", {
      nodeId: `nr-${seqResult}` as Id,
      revision: "1",
      operation: "close",
      baseRevision: null,
      toolCallId: "tc-bash" as Id,
      stage: "final",
      outcome: "succeeded",
      blocks: [{ type: "text", text: "zorpto-searchfind-4711" }],
      structuredResult: unknownKnowledge("text"),
      exitCode: known(0),
      changes: [],
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

function renderRouted(
  events: Observation[],
  path = "/s/ins_t",
  compact = true,
) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route
          path="/s/:instanceId"
          element={<Transcript events={events} compact={compact} />}
        />
      </Routes>
    </MemoryRouter>,
  );
}

describe("Transcript", () => {
  it("fills an empty completed message from delayed history without remounting its bubble", () => {
    const base = buildLongObservations({
      instanceId: "ins_stream",
      journalId: "obj_stream",
      hostId: "hst_1",
      count: 1,
    })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = {
      ...base,
      payload: {
        ...base.payload,
        status: "streaming" as const,
        blocks: [{ type: "text" as const, text: "hello from web hub" }],
      },
    };
    const close = {
      ...base,
      eventId: "evt_close",
      seq: "2",
      payload: {
        ...base.payload,
        messageId: "renamed",
        operation: "close" as const,
        revision: "2",
        baseRevision: "1",
        blocks: [],
      },
    };
    const { rerender } = render(
      <Transcript events={[close]} compact={false} />,
    );
    const bubble = screen.getByTestId("message");
    expect(bubble).toHaveTextContent("You");
    rerender(<Transcript events={[close, open]} compact={false} />);
    expect(screen.getAllByTestId("message")).toHaveLength(1);
    expect(screen.getByTestId("message")).toBe(bubble);
    expect(bubble).toHaveTextContent("hello from web hub");
  });

  it("updates the same assistant bubble while stream revisions arrive", () => {
    const base = buildLongObservations({
      instanceId: "ins_stream",
      journalId: "obj_stream",
      hostId: "hst_1",
      count: 1,
    })[0];
    if (base.kind !== "message") throw new Error("expected a message fixture");
    const open = {
      ...base,
      payload: {
        ...base.payload,
        role: "assistant" as const,
        status: "streaming" as const,
        blocks: [{ type: "text" as const, text: "我" }],
      },
    };
    const append = {
      ...open,
      eventId: "evt_append",
      seq: "2",
      payload: {
        ...open.payload,
        operation: "append" as const,
        revision: "2",
        baseRevision: "1",
        targetBlock: 0,
        blocks: [{ type: "text" as const, text: "先看看" }],
      },
    };
    const close = {
      ...open,
      eventId: "evt_close",
      seq: "3",
      payload: {
        ...open.payload,
        operation: "close" as const,
        revision: "3",
        baseRevision: "2",
        status: "complete" as const,
        blocks: [{ type: "text" as const, text: "我先看看" }],
      },
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
    const { container } = render(
      <Transcript events={events} compact={false} />,
    );
    await user.keyboard("j");
    expect(container.querySelector('[data-turn-active="1"]')).toBeTruthy();
    await user.keyboard("k");
    expect(container.querySelector("[data-anchor]")).toBeTruthy();
  });

  it("re-pins a pinned transcript to its tail when the scroller shrinks, and leaves a scrolled-up position alone", () => {
    // c-mfix round 4: the soft keyboard shrinks the scroller without changing
    // nodes/sizes. The ResizeObserver path must re-pin a following
    // transcript to the bottom and must NOT move a user who scrolled up.
    const ROW = 96;
    const COUNT = 60;
    const isScroller = (el: unknown) =>
      el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    let clientHeight = 720;
    const scrollHeight = COUNT * ROW;
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? clientHeight : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? scrollHeight : 0;
      },
    );
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
      height: ROW,
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      width: 0,
      x: 0,
      y: 0,
      toJSON() {},
    } as DOMRect);

    // One observer for the scroller; capture its callback so the test can
    // deliver the keyboard shrink the way the browser does.
    let fireScrollerResize: () => void = () => {};
    class ControllableResizeObserver {
      private cb: () => void;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(el: Element) {
        if (
          el instanceof HTMLElement &&
          el.dataset?.testid === "transcript-scroller"
        ) {
          fireScrollerResize = this.cb;
        }
      }
      unobserve() {}
      disconnect() {}
    }
    vi.stubGlobal("ResizeObserver", ControllableResizeObserver);

    const events = buildLongObservations({
      instanceId: "ins_shrink" as Id,
      journalId: "obj_shrink" as Id,
      hostId: "hst_1" as Id,
      count: COUNT,
    });
    render(<Transcript events={events} compact={false} />);
    const scroller = screen.getByTestId("transcript-scroller") as HTMLElement;

    // jsdom never clamps scrollTop to scrollHeight - clientHeight; a real
    // browser does, so emulate it on this element, starting at the pinned
    // mount position the layout effect already chose.
    let top = scrollHeight - clientHeight;
    Object.defineProperty(scroller, "scrollTop", {
      configurable: true,
      get: () => top,
      set: (v: number) => {
        top = Math.max(0, Math.min(v, scrollHeight - clientHeight));
      },
    });

    // Mount pins a follow session to the tail.
    expect((scroller as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      scrollHeight - 720,
    );

    // Keyboard shrinks the viewport from 720 to 323: pinned stays pinned.
    clientHeight = 323;
    fireScrollerResize();
    expect((scroller as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      scrollHeight - 323,
    );

    // User scrolls up (past the 64px pin threshold); a second shrink keeps
    // the reading position instead of yanking back to the tail.
    (scroller as HTMLElement & { scrollTop: number }).scrollTop = 100;
    fireEvent.scroll(scroller);
    clientHeight = 240;
    fireScrollerResize();
    expect((scroller as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      100,
    );

    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("does not rebuild row ResizeObservers on parent scroll re-renders", () => {
    const ROW = 96;
    const VIEW = 720;
    const isScroller = (el: unknown) =>
      el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? VIEW : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? 200 * ROW : 0;
      },
    );
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
      height: ROW,
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      width: 0,
      x: 0,
      y: 0,
      toJSON() {},
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
  // get a deterministic height (96, matching virtualWindow.DEFAULT_ROW; the
  // row's spacing is padding inside its box, so the rect is the whole row)
  // and the scroller a viewport/height.
  // Patches live on the prototypes so they also cover a remounted scroller.
  afterEach(() => {
    vi.restoreAllMocks();
  });

  function stubLayout(rowCount: number) {
    const ROW = 96;
    const VIEW = 720;
    const isScroller = (el: unknown) =>
      el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? VIEW : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return isScroller(this) ? rowCount * ROW : 0;
      },
    );
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
      height: ROW,
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      width: 0,
      x: 0,
      y: 0,
      toJSON() {},
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
    expect(screen.getByTestId("transcript-search-count").textContent).toMatch(
      /1\/1/,
    );

    await user.keyboard("[Enter]");
    // The hit is node index 1356, far below the ~24 rendered rows; the
    // scroller must have been positioned there from assembled geometry, never
    // by querying a DOM node the virtual window did not mount.
    expect((scroller as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      1356 * 96,
    );
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
    expect(localStorage.getItem("runtime.reading.v1.ins_restore")).toContain(
      '"follow":false',
    );

    renderRouted(events, "/s/ins_restore", false);
    const scroller2 = screen.getByTestId("transcript-scroller") as HTMLElement;
    expect((scroller2 as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      2400,
    );
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
    const saved = JSON.parse(
      localStorage.getItem("runtime.reading.v1.ins_follow") ?? "{}",
    );
    expect(saved.follow).toBe(true);
    const second = renderRouted(events, "/s/ins_follow", false);
    // A following transcript never restores an old offset and re-pins to end.
    const scroller2 = screen.getByTestId("transcript-scroller") as HTMLElement;
    expect((scroller2 as HTMLElement & { scrollTop: number }).scrollTop).toBe(
      60 * 96,
    );
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
    expect(screen.getByTestId("tool-failure-tag").textContent).toContain(
      "失败",
    );
    const card = screen.getByTestId("tool-card");
    expect(card.getAttribute("data-folded")).toBe("0");
    // Collapse-all folds routine cards; a failure must not disappear.
    await user.click(screen.getByTestId("collapse-all"));
    expect(screen.getByTestId("tool-failure-tag")).toBeTruthy();
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe(
      "0",
    );
  });

  it("keeps aria-live off on the transcript root while search is open", async () => {
    const user = userEvent.setup();
    renderRouted(
      [userMessage(1, "alpha"), assistantMessage(2, "beta")],
      "/s/ins_live",
    );
    const root = screen.getByTestId("transcript");
    expect(root.getAttribute("aria-live")).toBe("off");
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "beta");
    expect(root.getAttribute("aria-live")).toBe("off");
    expect(
      screen.getByTestId("transcript-search-count").getAttribute("aria-live"),
    ).toBe("off");
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

    // Search for text that exists ONLY in the tool result. The D-049 compact
    // fold hides the chip behind the ⋯ trigger on this layout; open it first.
    await user.click(screen.getByTestId("transcript-tools-open"));
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(
      screen.getByTestId("transcript-search-input"),
      "zorpto-searchfind-4711",
    );

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

  it("opens a nested settled subagent result that holds the only hit", async () => {
    const user = userEvent.setup();
    const executor = known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null });
    const call = (seq: number, id: string, name: string, input: unknown, parent: string | null) =>
      obs(seq, "tool_call", {
        nodeId: `nc-${seq}` as Id,
        revision: "1",
        operation: "open",
        baseRevision: null,
        toolCallId: id as Id,
        parentToolCallId: parent as Id | null,
        toolName: known(name),
        displayTitle: known(name),
        category: name === "Bash" ? "shell" : "agent",
        input: known(input),
        inputTextDelta: null,
        state: "running",
        executor,
      });
    const result = (seq: number, id: string, text: string) =>
      obs(seq, "tool_result", {
        nodeId: `nr-${seq}` as Id,
        revision: "1",
        operation: "close",
        baseRevision: null,
        toolCallId: id as Id,
        stage: "final",
        outcome: "succeeded",
        blocks: [{ type: "text", text }],
        structuredResult: unknownKnowledge("text"),
        exitCode: known(0),
        changes: [],
      });
    const child = call(3, "tc-child", "Bash", { command: "ls" }, "tc-task");
    const childResult = result(4, "tc-child", "quillon-nested-9921");
    for (const event of [child, childResult]) {
      (event.source as { nativeAgentId: unknown }).nativeAgentId = known("agent-sub");
    }
    renderRouted(
      [
        userMessage(1, "派个子任务"),
        call(2, "tc-task", "Task", { description: "look around" }, null),
        child,
        childResult,
        result(5, "tc-task", "sub done"),
        assistantMessage(6, "好了"),
      ],
      "/s/ins_nested_hit",
    );
    expect(screen.getByTestId("subagent-fold-toggle").getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("quillon-nested-9921")).toBeNull();

    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "quillon-nested-9921");

    // The hit opens the subagent fold AND the settled card inside it.
    await expect.poll(() => screen.queryByText("quillon-nested-9921")).not.toBeNull();
    const nested = screen
      .getByTestId("subagent-fold")
      .querySelector("[data-testid='tool-card']") as HTMLElement;
    expect(nested.getAttribute("data-folded")).toBe("0");
  });

  it("steps through two nested settled hits under one parent, opening each in turn", async () => {
    const user = userEvent.setup();
    renderRouted(
      [
        userMessage(1, "派个子任务"),
        nestedCall(2, "tc-task", "Task", { description: "look around" }, null),
        nestedCall(3, "tc-a", "Bash", { command: "ls a" }, "tc-task", "agent-sub"),
        nestedResult(4, "tc-a", "twinhit-3301 first", "agent-sub"),
        nestedCall(5, "tc-b", "Bash", { command: "ls b" }, "tc-task", "agent-sub"),
        nestedResult(6, "tc-b", "twinhit-3301 second", "agent-sub"),
        nestedResult(7, "tc-task", "sub done"),
        assistantMessage(8, "好了"),
      ],
      "/s/ins_twin_hit",
    );
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "twinhit-3301");
    const count = () => screen.getByTestId("transcript-search-count").textContent;
    const folded = () =>
      [...screen.getByTestId("subagent-fold").querySelectorAll("[data-testid='tool-card']")].map((card) =>
        card.getAttribute("data-folded"),
      );

    await expect.poll(count).toBe("1/2");
    await expect.poll(folded).toEqual(["0", "1"]);
    await user.click(screen.getByTestId("transcript-search-next"));
    expect(count()).toBe("2/2");
    await expect.poll(folded).toEqual(["1", "0"]);
    await user.click(screen.getByTestId("transcript-search-prev"));
    expect(count()).toBe("1/2");
    await expect.poll(folded).toEqual(["0", "1"]);
  });

  it("opens the workflow member list and card that hold the selected hit", async () => {
    const user = userEvent.setup();
    renderRouted(
      [
        userMessage(1, "跑个 workflow"),
        ...workflowRun(2, "tc-wf", "agent-m"),
        nestedCall(6, "tc-m1", "Bash", { command: "ls one" }, null, "agent-m"),
        nestedResult(7, "tc-m1", "memberhit-5120 one", "agent-m"),
        nestedCall(8, "tc-m2", "Bash", { command: "ls two" }, null, "agent-m"),
        nestedResult(9, "tc-m2", "memberhit-5120 two", "agent-m"),
        assistantMessage(10, "好了"),
      ],
      "/s/ins_member_hit",
    );
    const toggle = () => screen.getByTestId("workflow-member-tools-toggle");
    expect(toggle().getAttribute("aria-expanded")).toBe("false");

    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "memberhit-5120");
    const folded = () =>
      [...screen.getByTestId("workflow-card").querySelectorAll("[data-testid='tool-card']")].map((card) =>
        card.getAttribute("data-folded"),
      );
    await expect.poll(() => toggle().getAttribute("aria-expanded")).toBe("true");
    await expect.poll(folded).toEqual(["0", "1"]);
    await user.click(screen.getByTestId("transcript-search-next"));
    await expect.poll(folded).toEqual(["1", "0"]);
    // Leaving search hands the list back to the reader: closed again.
    await user.click(screen.getByTestId("transcript-search-close"));
    await expect.poll(() => toggle().getAttribute("aria-expanded")).toBe("false");
  });
});

describe("nested tool rows share the transcript expansion set", () => {
  it("全部折叠 closes an opened subagent fold and re-folds the card the reader expanded", async () => {
    const user = userEvent.setup();
    renderRouted(
      [
        userMessage(1, "派个子任务"),
        nestedCall(2, "tc-task", "Task", { description: "look around" }, null),
        nestedCall(3, "tc-a", "Bash", { command: "ls a" }, "tc-task", "agent-sub"),
        nestedResult(4, "tc-a", "nested body", "agent-sub"),
        nestedResult(5, "tc-task", "sub done"),
        assistantMessage(6, "好了"),
      ],
      "/s/ins_nested_collapse",
    );
    const toggle = () => screen.getByTestId("subagent-fold-toggle");
    await user.click(toggle());
    expect(toggle().getAttribute("aria-expanded")).toBe("true");
    const nested = () =>
      screen.getByTestId("subagent-fold").querySelector("[data-testid='tool-card']") as HTMLElement;
    expect(nested().getAttribute("data-folded")).toBe("1");
    fireEvent.click(nested().querySelector("[data-testid='tool-fold-open']") as HTMLElement);
    expect(nested().getAttribute("data-folded")).toBe("0");

    await user.click(screen.getByTestId("collapse-all"));
    expect(toggle().getAttribute("aria-expanded")).toBe("false");
    await user.click(toggle());
    expect(nested().getAttribute("data-folded")).toBe("1");
  });

  it("keeps a nested expansion when the parent row remounts", () => {
    const events = [
      userMessage(1, "派个子任务"),
      nestedCall(2, "tc-task", "Task", { description: "look around" }, null),
      nestedCall(3, "tc-a", "Bash", { command: "ls a" }, "tc-task", "agent-sub"),
      nestedResult(4, "tc-a", "nested body", "agent-sub"),
      nestedResult(5, "tc-task", "sub done"),
      assistantMessage(6, "好了"),
    ];
    const { rerender } = render(
      <MemoryRouter initialEntries={["/s/ins_nested_keep"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByTestId("subagent-fold-toggle"));
    const nested = () =>
      screen.getByTestId("subagent-fold").querySelector("[data-testid='tool-card']") as HTMLElement;
    fireEvent.click(nested().querySelector("[data-testid='tool-fold-open']") as HTMLElement);
    // Hide then re-show injected rows: the Task row leaves and re-enters the
    // mounted list the way a virtualised row does.
    rerender(
      <MemoryRouter initialEntries={["/s/ins_nested_keep"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events.slice(0, 1)} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.queryByTestId("subagent-fold")).toBeNull();
    rerender(
      <MemoryRouter initialEntries={["/s/ins_nested_keep"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.getByTestId("subagent-fold-toggle").getAttribute("aria-expanded")).toBe("true");
    expect(nested().getAttribute("data-folded")).toBe("0");
  });
});

const nestedExecutor = known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null });

/** A tool call, optionally stamped as a subagent's own (source agent id). */
function nestedCall(
  seq: number,
  id: string,
  name: string,
  input: unknown,
  parent: string | null,
  agent?: string,
): Observation {
  const event = obs(seq, "tool_call", {
    nodeId: `nc-${seq}` as Id,
    revision: "1",
    operation: "open",
    baseRevision: null,
    toolCallId: id as Id,
    parentToolCallId: parent as Id | null,
    toolName: known(name),
    displayTitle: known(name),
    category: name === "Bash" ? "shell" : name === "Workflow" ? "workflow" : "agent",
    input: known(input),
    inputTextDelta: null,
    state: "running",
    executor: nestedExecutor,
  });
  if (agent) (event.source as { nativeAgentId: unknown }).nativeAgentId = known(agent);
  return event;
}

function nestedResult(seq: number, id: string, text: string, agent?: string): Observation {
  const event = obs(seq, "tool_result", {
    nodeId: `nr-${seq}` as Id,
    revision: "1",
    operation: "close",
    baseRevision: null,
    toolCallId: id as Id,
    stage: "final",
    outcome: "succeeded",
    blocks: [{ type: "text", text }],
    structuredResult: unknownKnowledge("text"),
    exitCode: known(0),
    changes: [],
  });
  if (agent) (event.source as { nativeAgentId: unknown }).nativeAgentId = known(agent);
  return event;
}

/** A Workflow tool row with one running phase and one member agent (4 events). */
function workflowRun(seq: number, toolCallId: string, agent: string): Observation[] {
  const wfId = "wf_nested" as Id;
  return [
    nestedCall(seq, toolCallId, "Workflow", { name: "nested" }, null),
    obs(seq + 1, "workflow.run", {
      workflowId: wfId,
      engine: "claude-workflow",
      nativeRunId: known("wf_x"),
      nativeTaskId: known("task-1"),
      toolCallId: toolCallId as Id,
      state: "running",
      revision: "1",
      title: known("nested"),
      resultRef: null,
    }),
    obs(seq + 2, "workflow.phase", {
      workflowId: wfId,
      phaseId: "ph1" as Id,
      nativePhaseId: known("Review"),
      label: known("Review"),
      state: "running",
      revision: "1",
      parentPhaseId: null,
    }),
    obs(seq + 3, "workflow.member", {
      workflowId: wfId,
      memberId: "mem_a" as Id,
      nativeAgentId: known(agent),
      nativeKey: known("key-a"),
      attempt: known("1"),
      phaseId: "ph1" as Id,
      label: known("review:nested"),
      state: "running",
      modelRequested: known("claude-opus-5"),
      modelResolved: known("claude-opus-5"),
      resultRef: null,
      revision: "1",
      latestTool: known("Bash"),
      tokens: "1000",
      calls: "2",
      durationMs: null,
      startedAt: null,
      endedAt: null,
    }),
  ];
}

/** A user-role message the agent did not write: hook context injection. */
function injectedMessage(seq: number, text: string): Observation {
  const event = userMessage(seq, text);
  return {
    ...event,
    payload: {
      ...(event.payload as Record<string, unknown>),
      origin: "hook-context",
    },
  } as Observation;
}

describe("D-049 compact transcript toolbar fold", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("folds the three action chips behind one trigger and reaches all three testids after expanding", async () => {
    stubCompactLayout();
    const user = userEvent.setup();
    renderRouted(
      [
        userMessage(1, "跑一下"),
        injectedMessage(2, "injected hook body msesfold-4711"),
        assistantMessage(3, "done"),
      ],
      "/s/ins_fold",
    );

    // Folded: just the trigger; the three chips are not mounted at all.
    const trigger = screen.getByTestId("transcript-tools-open");
    expect(trigger).toBeTruthy();
    expect(screen.queryByTestId("collapse-all")).toBeNull();
    expect(screen.queryByTestId("transcript-search-open")).toBeNull();
    expect(screen.queryByTestId("toggle-injected")).toBeNull();
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    // Plain disclosure, not a menu: the chips expand inline in its place.
    expect(trigger.getAttribute("aria-haspopup")).toBeNull();
    expect(trigger.textContent).toContain("⋯");

    // Expanding mounts the exact same buttons with the unchanged testids.
    await user.click(trigger);
    expect(screen.getByTestId("collapse-all")).toBeTruthy();
    expect(screen.getByTestId("transcript-search-open")).toBeTruthy();
    const injected = screen.getByTestId("toggle-injected");
    expect(injected.textContent).toContain("注入内容 · 1");
  });

  it("re-folds the row after an action is chosen", async () => {
    stubCompactLayout();
    const user = userEvent.setup();
    renderRouted(
      [userMessage(1, "跑一下"), assistantMessage(2, "done")],
      "/s/ins_refold",
    );

    await user.click(screen.getByTestId("transcript-tools-open"));
    // 搜索正文 opens the dedicated search strip and folds the chips again.
    await user.click(screen.getByTestId("transcript-search-open"));
    expect(screen.getByTestId("transcript-search-input")).toBeTruthy();
    expect(screen.getByTestId("transcript-tools-open")).toBeTruthy();
    expect(screen.queryByTestId("transcript-search-open")).toBeNull();

    // 全部折叠 likewise leaves the single trigger, not the expanded row.
    await user.click(screen.getByTestId("transcript-tools-open"));
    await user.click(screen.getByTestId("collapse-all"));
    expect(screen.getByTestId("transcript-tools-open")).toBeTruthy();
    expect(screen.queryByTestId("collapse-all")).toBeNull();
  });

  it("keeps the chips inline with no trigger on a desktop-width layout", () => {
    // No matchMedia stub: jsdom has no matchMedia, which reads as the desktop
    // default (same guard ToolCard's layout hook uses).
    renderRouted(
      [userMessage(1, "跑一下"), assistantMessage(2, "done")],
      "/s/ins_wide",
      false,
    );
    expect(screen.getByTestId("collapse-all")).toBeTruthy();
    expect(screen.getByTestId("transcript-search-open")).toBeTruthy();
    expect(screen.queryByTestId("transcript-tools-open")).toBeNull();
  });
});

/** One streaming assistant message: open, then text appends, then close. */
function streamingMessage(seq: number, text: string, revision: number): Observation {
  const event = assistantMessage(seq, text);
  const payload = event.payload as Record<string, unknown>;
  return {
    ...event,
    payload: {
      ...payload,
      nodeId: "n-stream" as Id,
      messageId: "m-stream" as Id,
      status: "streaming",
      operation: revision === 1 ? "open" : "append",
      revision: String(revision),
      baseRevision: revision === 1 ? null : String(revision - 1),
      targetBlock: revision === 1 ? null : 0,
    },
  } as Observation;
}

function closeStreaming(seq: number, text: string, revision: number): Observation {
  const event = streamingMessage(seq, text, revision);
  return {
    ...event,
    payload: {
      ...(event.payload as Record<string, unknown>),
      operation: "close",
      status: "complete",
      targetBlock: null,
    },
  } as Observation;
}

describe("TranscriptHandle (D-053)", () => {
  it("opens search and collapses every expanded card through the ref", () => {
    const ref = createRef<TranscriptHandle>();
    render(
      <MemoryRouter initialEntries={["/s/ins_handle"]}>
        <Routes>
          <Route
            path="/s/:instanceId"
            element={
              <Transcript
                ref={ref}
                events={[userMessage(1, "跑一下"), ...settledBash(2, 3), assistantMessage(4, "done")]}
                compact={false}
              />
            }
          />
        </Routes>
      </MemoryRouter>,
    );
    expect(ref.current).not.toBeNull();
    // Desktop reading column: the settled card starts folded.
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");
    fireEvent.click(screen.getByTestId("tool-fold-open"));
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("0");
    act(() => ref.current?.collapseAll());
    expect(screen.getByTestId("tool-card").getAttribute("data-folded")).toBe("1");

    expect(screen.queryByTestId("transcript-search-input")).toBeNull();
    act(() => ref.current?.openSearch());
    expect(screen.getByTestId("transcript-search-input")).toBeTruthy();
  });

  it("shows the toolbar by default and hides it with toolbar={false}", () => {
    const events = [userMessage(1, "alpha"), assistantMessage(2, "beta")];
    const { unmount } = render(<Transcript events={events} compact={false} />);
    expect(screen.getByTestId("transcript-toolbar")).toBeTruthy();
    unmount();
    const ref = createRef<TranscriptHandle>();
    render(<Transcript ref={ref} events={events} compact={false} toolbar={false} />);
    expect(screen.queryByTestId("transcript-toolbar")).toBeNull();
    expect(screen.queryByTestId("collapse-all")).toBeNull();
    // The handle still drives search when the host owns the controls.
    act(() => ref.current?.openSearch());
    expect(screen.getByTestId("transcript-search-input")).toBeTruthy();
  });
});

describe("streaming row (D-053)", () => {
  afterEach(() => {
    profile.on = false;
    profile.probes = [];
  });

  it("commits only the streaming row per batch", () => {
    profile.on = true;
    const settled = [userMessage(1, "问题"), assistantMessage(2, "上一轮回答"), userMessage(3, "继续")];
    const first = streamingMessage(4, "第一段", 1);
    const { rerender } = render(<Transcript events={[...settled, first]} compact={false} />);
    for (let rev = 2; rev <= 4; rev += 1) {
      profile.probes = [];
      rerender(
        <Transcript
          events={[
            ...settled,
            first,
            ...Array.from({ length: rev - 1 }, (_, i) => streamingMessage(5 + i, ` 追加${i}`, i + 2)),
          ]}
          compact={false}
        />,
      );
      const rows = profile.probes
        .filter((p) => p.kind === "commit:TranscriptRow")
        .map((p) => (p.value as { nodeId: string }).nodeId);
      expect(rows.length).toBeGreaterThan(0);
      expect(new Set(rows).size).toBe(1);
    }
  });

  it("shows the caret while streaming and removes it on close without touching the text", () => {
    const open = streamingMessage(1, "```ts\nconst a = 1;", 1);
    const { rerender } = render(<Transcript events={[open]} compact={false} />);
    expect(screen.getByTestId("streaming-cursor")).toBeTruthy();
    // An open fence renders closed while streaming, so the block is already
    // a code block and does not reflow when the closing fence arrives.
    expect(screen.getByTestId("code-block")).toBeTruthy();
    rerender(
      <Transcript events={[open, closeStreaming(2, "```ts\nconst a = 1;\n```", 2)]} compact={false} />,
    );
    expect(screen.queryByTestId("streaming-cursor")).toBeNull();
    expect(screen.getByTestId("code-block")).toBeTruthy();
  });

  it("re-commits only held rows when the steer control flips", () => {
    profile.on = true;
    const events = [userMessage(1, "问题"), assistantMessage(2, "回答")];
    const held = {
      clientRequestId: "req-held" as Id,
      instanceId: "ins_steer" as Id,
      text: "排队的消息",
      commandId: null,
      state: "queued" as const,
      createdAt: "2026-09-24T00:00:00Z",
      held: true,
      holdReason: "turn" as const,
    };
    const { rerender } = render(
      <Transcript events={events} bubbles={[held]} compact={false} steerHeld={{ enabled: false, reason: "" }} />,
    );
    expect((screen.getByTestId("held-queue-steer") as HTMLButtonElement).disabled).toBe(true);
    profile.probes = [];
    rerender(
      <Transcript events={events} bubbles={[held]} compact={false} steerHeld={{ enabled: true, reason: "" }} />,
    );
    const rows = profile.probes
      .filter((p) => p.kind === "commit:TranscriptRow")
      .map((p) => (p.value as { nodeId: string }).nodeId);
    expect(new Set(rows).size).toBe(1);
    expect((screen.getByTestId("held-queue-steer") as HTMLButtonElement).disabled).toBe(false);
  });
});
