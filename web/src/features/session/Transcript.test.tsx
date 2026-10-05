import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act, createRef, useRef, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter, Route, Routes, useNavigate, useParams } from "react-router-dom";
import { buildLongObservations } from "../../fixtures/session/longEvents";
import type { Observation, Snapshot } from "../../types/observation";
import { known, unknownKnowledge, type Id } from "../../types/wire";
import type { LocalBubble } from "../../lib/store";
import { hubStore } from "../../lib/store";
import { JournalClient, type JournalRead } from "../../lib/journal";
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

  it("shows a rejected bubble's stored reason inline next to 未送达 (ROUND4-4), not a toast", () => {
    const rejectedBubble: LocalBubble = {
      clientRequestId: "local_rej",
      instanceId: "ins_t",
      text: "doomed steer",
      commandId: "cmd_rej",
      state: "unknown",
      outboxState: "rejected",
      outboxError: "turn does not exist",
      createdAt: "2026-09-24T00:00:00.000Z",
    };
    render(<Transcript events={[]} bubbles={[rejectedBubble]} compact={false} />);
    const row = screen.getByTestId("optimistic-bubble");
    expect(row.textContent).toContain("未送达");
    // The Node/Hub's own reason is neutral inline text in the same row.
    const reason = screen.getByTestId("send-rejected-reason");
    expect(reason.textContent).toContain("turn does not exist");
    expect(row.contains(reason)).toBe(true);
  });

  it("shows 未送达 without a reason suffix when the rejected row carries none (ROUND4-4)", () => {
    const rejectedBubble: LocalBubble = {
      clientRequestId: "local_rej2",
      instanceId: "ins_t",
      text: "doomed no reason",
      commandId: "cmd_rej2",
      state: "unknown",
      outboxState: "rejected",
      createdAt: "2026-09-24T00:00:00.000Z",
    };
    render(<Transcript events={[]} bubbles={[rejectedBubble]} compact={false} />);
    expect(screen.getByTestId("optimistic-bubble").textContent).toContain("未送达");
    expect(screen.queryByTestId("send-rejected-reason")).toBeNull();
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
    const { rerender } = render(<Transcript events={[...settled, streamingMessage(4, "第一段", 1)]} compact={false} />);
    const streamingId = (screen.getByTestId("streaming-cursor").closest("[data-anchor]") as HTMLElement).dataset
      .anchor;
    expect(streamingId).toBeTruthy();
    let text = "第一段";
    for (let batch = 1; batch <= 3; batch += 1) {
      profile.probes = [];
      // The same node, revised with longer text: one message, not new ones.
      text += ` 追加${batch}`;
      rerender(<Transcript events={[...settled, streamingMessage(4, text, 1)]} compact={false} />);
      expect(screen.getByText(text)).toBeTruthy();
      expect(screen.getAllByTestId("message")).toHaveLength(4);
      const rows = profile.probes
        .filter((p) => p.kind === "commit:TranscriptRow")
        .map((p) => (p.value as { nodeId: string }).nodeId);
      expect(rows.length).toBeGreaterThan(0);
      expect([...new Set(rows)]).toEqual([streamingId]);
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

// ---------------------------------------------------------------------------
// load-earlier through the REAL JournalClient -> Transcript path (UO-6a r4).
// A scripted bounded read drives a real JournalClient; its onPrepend feeds the
// rendered Transcript exactly like hubStore.follow's listener, so anchor
// completion, scroll cancellation and cross-session identity are exercised
// end to end.
// ---------------------------------------------------------------------------
describe("load-earlier paging via JournalClient (UO-6a r4)", () => {
  const ROW = 96;
  const VIEW = 720;

  function toolPair(seqCall: number, tcId: string, instanceId: string): Observation[] {
    const s = source();
    return [
      {
        schemaVersion: 1,
        eventId: `evt_${seqCall}` as Id,
        journalId: `obj_${instanceId}` as Id,
        instanceId: instanceId as Id,
        runId: null,
        hostId: "hst" as Id,
        processGeneration: "1",
        runGeneration: null,
        seq: String(seqCall),
        observedAt: "2026-09-12T00:00:00.000Z",
        nativeAt: known("2026-09-12T00:00:00.000Z"),
        source: s,
        kind: "tool_call",
        completeness: "structured",
        rawRef: null,
        evidenceEventIds: [],
        payload: {
          nodeId: `n_${tcId}_c` as Id,
          revision: "1",
          operation: "open",
          baseRevision: null,
          toolCallId: tcId as Id,
          parentToolCallId: null,
          toolName: known("Bash"),
          displayTitle: known("Bash"),
          category: "shell",
          input: known({ command: `cmd ${tcId}` }),
          inputTextDelta: null,
          state: "running",
          executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
        },
      } as Observation,
      {
        schemaVersion: 1,
        eventId: `evt_${seqCall + 1}` as Id,
        journalId: `obj_${instanceId}` as Id,
        instanceId: instanceId as Id,
        runId: null,
        hostId: "hst" as Id,
        processGeneration: "1",
        runGeneration: null,
        seq: String(seqCall + 1),
        observedAt: "2026-09-12T00:00:00.000Z",
        nativeAt: known("2026-09-12T00:00:00.000Z"),
        source: s,
        kind: "tool_result",
        completeness: "structured",
        rawRef: null,
        evidenceEventIds: [],
        payload: {
          nodeId: `n_${tcId}_r` as Id,
          revision: "1",
          operation: "close",
          baseRevision: null,
          toolCallId: tcId as Id,
          stage: "final",
          outcome: "succeeded",
          blocks: [{ type: "text", text: `out ${tcId}` }],
          structuredResult: unknownKnowledge("text"),
          exitCode: known(0),
          changes: [],
        },
      } as Observation,
    ];
  }

  function source() {
    return {
      driverKind: "claude-print" as const,
      driverVersion: "1",
      adapterVersion: "1",
      channel: "stdout" as const,
      delivery: "replay" as const,
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" as const },
      sourceCursor: { type: "runtime" as const, ledgerRevision: "1" },
    };
  }

  function msg(seq: number, role: "user" | "assistant", instanceId: string, text = `m${seq}`): Observation {
    return {
      schemaVersion: 1,
      eventId: `evt_${seq}_${role}` as Id,
      journalId: `obj_${instanceId}` as Id,
      instanceId: instanceId as Id,
      runId: null,
      hostId: "hst" as Id,
      processGeneration: "1",
      runGeneration: null,
      seq: String(seq),
      observedAt: "2026-09-12T00:00:00.000Z",
      nativeAt: known("2026-09-12T00:00:00.000Z"),
      source: source(),
      kind: "message",
      completeness: "structured",
      rawRef: null,
      evidenceEventIds: [],
      payload: {
        nodeId: `n_${seq}_${role}` as Id,
        messageId: `m_${seq}_${role}` as Id,
        role,
        phase: role === "user" ? "input" : "final",
        revision: "1",
        baseRevision: null,
        operation: "open",
        blocks: [{ type: "text", text }],
        targetBlock: null,
        parentToolCallId: null,
        nativeOrigin: known(role === "user" ? "ui" : "assistant"),
        status: "complete",
      },
    } as Observation;
  }

  function pageOf(events: Observation[], reachedAfterSeq = false) {
    return {
      events,
      durableSeq: events.at(-1)?.seq ?? "0",
      windowFromSeq: events[0]?.seq ?? null,
      reachedAfterSeq,
    };
  }

  type Registry = {
    events: Record<string, Observation[]>;
    floors: Record<string, string>;
    clients: Record<string, JournalClient>;
    setEvents: Record<string, (events: Observation[]) => void>;
    setFloor: Record<string, (floor: string) => void>;
  };

  function makeClient(
    reg: Registry,
    instanceId: string,
    initial: Observation[],
    read: JournalRead,
    snapshotFloor: string,
    asOf: string,
  ): JournalClient {
    const client = new JournalClient(`obj_${instanceId}` as Id, read, {
      onPrepend: (rows) => {
        const merged = (reg.events[instanceId] ?? []).concat(rows).sort((a, b) => Number(a.seq) - Number(b.seq));
        reg.setEvents[instanceId]?.(merged);
      },
    });
    client.noteHistory(initial);
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: "ep" as Id,
      asOfSeq: asOf,
      instance: {} as Snapshot["instance"],
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: snapshotFloor, complete: false },
    });
    reg.clients[instanceId] = client;
    reg.events[instanceId] = initial;
    reg.floors[instanceId] = snapshotFloor;
    return client;
  }

  function Driver({ reg }: { reg: Registry }) {
    const { instanceId = "" } = useParams();
    const [events, setEvents] = useState<Observation[]>(reg.events[instanceId] ?? []);
    const [floor, setFloor] = useState<string>(reg.floors[instanceId] ?? "1");
    const activeRef = useRef(instanceId);
    activeRef.current = instanceId;
    reg.setEvents[instanceId] = (next) => {
      // A late prepend from another session's in-flight click must not paint
      // over the route the reader switched to.
      if (activeRef.current !== instanceId) return;
      reg.events[instanceId] = next;
      setEvents(next);
    };
    reg.setFloor[instanceId] = (next) => {
      if (activeRef.current !== instanceId) return;
      reg.floors[instanceId] = next;
      setFloor(next);
    };
    return <Transcript events={events} earlierFloor={floor} compact />;
  }

  function GoTo({ to }: { to: string }) {
    const navigate = useNavigate();
    return (
      <button type="button" data-testid={`go-${to}`} onClick={() => navigate(to)}>
        go
      </button>
    );
  }

  function renderDriver(reg: Registry, path: string) {
    return render(
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route
            path="/s/:instanceId"
            element={
              <>
                <Driver reg={reg} />
                <GoTo to="/s/insB" />
              </>
            }
          />
        </Routes>
      </MemoryRouter>,
    );
  }

  /** Deterministic flat row geometry off the virtual window's pad spacer. */
  function installGeometry(totalCount: number) {
    const heights = new WeakMap<Element, number>();
    const observerCbs = new Map<Element, () => void>();
    const isScroller = (el: unknown) => el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    let top = 0;
    // Full-list scroll height (the existing pin tests do the same): at the
    // scroll event the DOM still shows the PREVIOUS virtual window, and
    // deriving scrollHeight from it flips pinRef for any valid middle offset.
    const listHeight = () => totalCount * ROW;
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? VIEW : 0;
    });
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? listHeight() : 0;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
      const el = this as HTMLElement;
      const h = heights.get(el) ?? ROW;
      if (el.dataset?.testid === "transcript-scroller") {
        return { top: 0, left: 0, right: 500, bottom: VIEW, width: 500, height: VIEW, x: 0, y: 0, toJSON() {} } as DOMRect;
      }
      if (el.dataset?.anchor && el.parentElement) {
        const list = el.parentElement;
        const spacer = Array.from(list.children).find((c) => c.getAttribute("aria-hidden") === "true") as
          | HTMLElement
          | undefined;
        const pad = Number.parseFloat(spacer?.style.height ?? "0") || 0;
        // Sum the MEASURED heights of every mounted sibling above this row: a
        // grown row above contributes its extra height to every later rect.
        let preceding = 0;
        for (const sibling of Array.from(list.querySelectorAll("[data-anchor]"))) {
          if (sibling === el) break;
          preceding += heights.get(sibling) ?? ROW;
        }
        const rowTop = pad + preceding - top;
        return { top: rowTop, left: 0, right: 500, bottom: rowTop + h, width: 500, height: h, x: 0, y: rowTop, toJSON() {} } as DOMRect;
      }
      return { top: 0, left: 0, right: 0, bottom: h, width: 0, height: h, x: 0, y: 0, toJSON() {} } as DOMRect;
    });
    class GeometryRO {
      private readonly cb: () => void;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(el: Element) {
        observerCbs.set(el, this.cb);
      }
      unobserve(el: Element) {
        observerCbs.delete(el);
      }
      disconnect() {
        observerCbs.clear();
      }
    }
    vi.stubGlobal("ResizeObserver", GeometryRO);

    const scroller = () => screen.getByTestId("transcript-scroller") as HTMLElement;
    const defineScroll = () => {
      const el = scroller();
      Object.defineProperty(el, "scrollTop", {
        configurable: true,
        get: () => top,
        set: (v: number) => {
          top = v;
        },
      });
    };
    const scrollTo = (value: number) => {
      top = value;
      fireEvent.scroll(scroller());
    };
    const growMountedRow = (ordinal: number, height: number) => {
      const el = scroller().querySelectorAll<HTMLElement>("[data-anchor]")[ordinal];
      if (!el) throw new Error(`mounted row ${ordinal} not found`);
      heights.set(el, height);
      observerCbs.get(el)?.();
    };
    const scrollTopNow = () => top;
    return { scroller, defineScroll, scrollTo, growMountedRow, scrollTopNow };
  }

  function gate<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => {
      resolve = r;
    });
    return { promise, resolve };
  }

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("retires a load-earlier anchor whose compact fold was renamed by the older page (item 1)", async () => {
    const user = userEvent.setup();
    const geo = installGeometry(39);
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    // Window starts mid-turn: three tools (one compact fold) then an assistant,
    // followed by enough turns to scroll into later.
    const initial: Observation[] = [
      ...toolPair(201, "tc-a", "ins1"),
      ...toolPair(203, "tc-b", "ins1"),
      ...toolPair(205, "tc-c", "ins1"),
      msg(207, "assistant", "ins1"),
    ];
    // Thirty more message nodes: a long list the reader can sit inside
    // without being pinned to the tail (jsdom does not clamp scrollTop).
    for (let seq = 208, i = 0; i < 15; i += 1, seq += 2) {
      initial.push(msg(seq, "user", "ins1"));
      initial.push(msg(seq + 1, "assistant", "ins1"));
    }
    // The older page continues the SAME tool run with earlier tool_call ids:
    // the fold compact:tc-a is renamed to compact:tc-x (the armed id vanishes).
    const olderPage = [
      ...toolPair(101, "tc-x", "ins1"),
      ...toolPair(103, "tc-y", "ins1"),
    ];
    makeClient(
      reg,
      "ins1",
      initial,
      vi.fn<JournalRead>().mockResolvedValue(pageOf(olderPage)),
      "201",
      "225",
    );
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });

    renderDriver(reg, "/s/ins1");
    geo.defineScroll();
    const scroller = geo.scroller();
    // Mount pins a follow session to the tail; scroll to the fold at the top.
    geo.scrollTo(0);
    expect(scroller.querySelector("[data-anchor='compact:tc-a']")).toBeTruthy();
    await user.click(screen.getByTestId("load-earlier"));
    await act(async () => {});
    // The renamed fold sits where the armed fold was.
    expect(scroller.querySelector("[data-anchor='compact:tc-x']")).toBeTruthy();

    // Drive four stable passes (mount measure + three commits from rows below
    // the anchor) so the held restore converges and releases.
    await act(async () => {});
    geo.growMountedRow(5, ROW + 1);
    geo.growMountedRow(6, ROW + 2);
    geo.growMountedRow(7, ROW + 3);
    geo.growMountedRow(5, ROW);
    geo.growMountedRow(6, ROW);
    geo.growMountedRow(7, ROW);

    // After release, ordinary growth anchoring works again: scroll into the
    // list, sample a reading anchor, then grow a mounted row ABOVE it.
    // Sit at a genuine reading position: 40+ nodes, viewport 720, not pinned.
    geo.scrollTo(25 * ROW);
    await act(async () => {});
    const before = geo.scrollTopNow();
    // Grow a mounted row ABOVE the sampled reading anchor. Once the prepend
    // restore has RELEASED, holdReadingAnchor compensates (a leaked restore
    // disables that hold for the whole session and leaves the growth ignored).
    geo.growMountedRow(4, ROW + 40);
    expect(geo.scrollTopNow()).toBeGreaterThanOrEqual(before + 39);
  });

  it("cancels the held restore when the reader scrolls before the page lands (item 3)", async () => {
    const user = userEvent.setup();
    const geo = installGeometry(40);
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    const initial: Observation[] = [];
    for (let i = 0; i < 20; i += 1) {
      initial.push(msg(1001 + i, i % 2 === 0 ? "user" : "assistant", "ins3", `m${1001 + i}`));
    }
    const older: Observation[] = [];
    for (let i = 0; i < 20; i += 1) older.push(msg(901 + i, i % 2 === 0 ? "user" : "assistant", "ins3", `o${901 + i}`));
    const g = gate<ReturnType<typeof pageOf>>();
    makeClient(reg, "ins3", initial, vi.fn<JournalRead>().mockImplementation(() => g.promise), "1001", "1020");
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });

    renderDriver(reg, "/s/ins3");
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));
    // The reader navigates manually while the older window is still in flight.
    geo.scrollTo(300);
    await act(async () => {
      g.resolve(pageOf(older));
      await Promise.resolve();
    });
    await act(async () => {});
    // The prepend must not yank the scroller back to the click-time row.
    expect(geo.scrollTopNow()).toBe(300);
  });

  it("a late resolve from session A cannot clear session B's armed restore (item 4)", async () => {
    const user = userEvent.setup();
    const geo = installGeometry(40);
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    const mkTurns = (instanceId: string) => {
      const out: Observation[] = [];
      for (let i = 0; i < 20; i += 1) {
        out.push(msg(1001 + i, i % 2 === 0 ? "user" : "assistant", instanceId, `${instanceId}-${1001 + i}`));
      }
      return out;
    };
    // A reconnect re-anchor above held rows lets A's click resolve
    // duplicate-only (rows all already held), which is the finalize path that
    // used to clear whatever anchors were armed next.
    const gateA = gate<ReturnType<typeof pageOf>>();
    makeClient(
      reg,
      "insA",
      mkTurns("insA"),
      vi.fn<JournalRead>().mockImplementation(() => gateA.promise),
      "1021",
      "1040",
    );
    // Seed rows under the higher re-anchor floor.
    reg.clients.insA.noteHistory(mkTurns("insA"));
    const gateB = gate<ReturnType<typeof pageOf>>();
    const olderB: Observation[] = [];
    for (let i = 0; i < 20; i += 1) {
      olderB.push(msg(901 + i, i % 2 === 0 ? "user" : "assistant", "insB", `b-${901 + i}`));
    }
    makeClient(
      reg,
      "insB",
      mkTurns("insB"),
      vi.fn<JournalRead>().mockImplementation(() => gateB.promise),
      "1001",
      "1020",
    );
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });

    renderDriver(reg, "/s/insA");
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));

    // Route to B (same mounted Transcript) and arm B while A is still in flight.
    await user.click(screen.getByTestId("go-/s/insB"));
    geo.scrollTo(0);
    const buttonB = screen.getByTestId("load-earlier");
    await user.click(buttonB);
    expect((buttonB as HTMLButtonElement).disabled).toBe(true);

    // A resolves duplicate-only: 1001..1020 below the re-anchored floor 1021
    // is every row A already holds.
    await act(async () => {
      gateA.resolve(pageOf(mkTurns("insA")));
      await Promise.resolve();
    });
    // B's request is still the owner: loading state survives.
    expect((screen.getByTestId("load-earlier") as HTMLButtonElement).disabled).toBe(true);

    // B's real prepend lands and restores the click-time anchor (now 20 rows
    // deeper) instead of having been cleared by A.
    await act(async () => {
      gateB.resolve(pageOf(olderB));
      await Promise.resolve();
    });
    await act(async () => {});
    expect(Math.abs(geo.scrollTopNow() - 20 * ROW)).toBeLessThanOrEqual(4);
  });
});

describe("load-earlier anchor lifecycle round 5", () => {
  const ROW = 96;
  const VIEW = 720;

  function source() {
    return {
      driverKind: "claude-print" as const,
      driverVersion: "1",
      adapterVersion: "1",
      channel: "stdout" as const,
      delivery: "replay" as const,
      nativeSessionId: unknownKnowledge("none"),
      nativeTurnId: unknownKnowledge("none"),
      nativeAgentId: unknownKnowledge("none"),
      nativeItemId: unknownKnowledge("none"),
      nativeEventId: unknownKnowledge("none"),
      nativeRequestId: { type: "none" as const },
      sourceCursor: { type: "runtime" as const, ledgerRevision: "1" },
    };
  }

  function m(seq: number, role: "user" | "assistant", instanceId: string): Observation {
    return {
      schemaVersion: 1,
      eventId: `e_${seq}_${role}_${instanceId}` as Id,
      journalId: `obj_${instanceId}` as Id,
      instanceId: instanceId as Id,
      runId: null,
      hostId: "hst" as Id,
      processGeneration: "1",
      runGeneration: null,
      seq: String(seq),
      observedAt: "2026-09-12T00:00:00.000Z",
      nativeAt: known("2026-09-12T00:00:00.000Z"),
      source: source(),
      kind: "message",
      completeness: "structured",
      rawRef: null,
      evidenceEventIds: [],
      payload: {
        nodeId: `n_${seq}_${role}_${instanceId}` as Id,
        messageId: `mm_${seq}_${role}_${instanceId}` as Id,
        role,
        phase: role === "user" ? "input" : "final",
        revision: "1",
        baseRevision: null,
        operation: "open",
        blocks: [{ type: "text", text: `${instanceId}-m${seq}` }],
        targetBlock: null,
        parentToolCallId: null,
        nativeOrigin: known(role === "user" ? "ui" : "assistant"),
        status: "complete",
      },
    } as Observation;
  }

  function pageOf(events: Observation[], reachedAfterSeq = false) {
    return {
      events,
      durableSeq: events.at(-1)?.seq ?? "0",
      windowFromSeq: events[0]?.seq ?? null,
      reachedAfterSeq,
    };
  }

  type Registry = {
    events: Record<string, Observation[]>;
    floors: Record<string, string>;
    clients: Record<string, JournalClient>;
    setEvents: Record<string, (events: Observation[]) => void>;
    setFloor: Record<string, (floor: string) => void>;
  };

  function makeClient(
    reg: Registry,
    instanceId: string,
    initial: Observation[],
    read: JournalRead,
    snapshotFloor: string,
    asOf: string,
  ): JournalClient {
    const client = new JournalClient(`obj_${instanceId}` as Id, read, {
      onPrepend: (rows) => {
        const merged = (reg.events[instanceId] ?? []).concat(rows).sort((a, b) => Number(a.seq) - Number(b.seq));
        reg.setEvents[instanceId]?.(merged);
      },
    });
    client.noteHistory(initial);
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: "ep" as Id,
      asOfSeq: asOf,
      instance: {} as Snapshot["instance"],
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: snapshotFloor, complete: false },
    });
    reg.clients[instanceId] = client;
    reg.events[instanceId] = initial;
    reg.floors[instanceId] = snapshotFloor;
    return client;
  }

  function Driver({ reg, compact = true }: { reg: Registry; compact?: boolean }) {
    const { instanceId = "" } = useParams();
    const [sessionId, setSessionId] = useState(instanceId);
    const [events, setEvents] = useState<Observation[]>(reg.events[instanceId] ?? []);
    const [floor, setFloor] = useState<string>(reg.floors[instanceId] ?? "1");
    // Route param change WITHOUT a remount (Transcript itself stays mounted,
    // exactly like SessionPage): swap the store-backed state in render.
    if (sessionId !== instanceId) {
      setSessionId(instanceId);
      setEvents(reg.events[instanceId] ?? []);
      setFloor(reg.floors[instanceId] ?? "1");
    }
    const activeRef = useRef(instanceId);
    activeRef.current = instanceId;
    reg.setEvents[instanceId] = (next) => {
      if (activeRef.current !== instanceId) return;
      reg.events[instanceId] = next;
      setEvents(next);
    };
    reg.setFloor[instanceId] = (next) => {
      if (activeRef.current !== instanceId) return;
      reg.floors[instanceId] = next;
      setFloor(next);
    };
    return <Transcript events={events} earlierFloor={floor} compact={compact} />;
  }

  function installGeo(totalCount: number, opts: { echoOnWrite?: boolean; onWrite?: (v: number) => void } = {}) {
    const echoOnWrite = opts.echoOnWrite ?? false;
    let dynamicTotal = totalCount;
    const heights = new WeakMap<Element, number>();
    const observerCbs = new Map<Element, () => void>();
    const isScroller = (el: unknown) => el instanceof HTMLElement && el.dataset?.testid === "transcript-scroller";
    let top = 0;
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? VIEW : 0;
    });
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
      return isScroller(this) ? dynamicTotal * ROW : 0;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
      const el = this as HTMLElement;
      const h = heights.get(el) ?? ROW;
      if (el.dataset?.testid === "transcript-scroller") {
        return { top: 0, left: 0, right: 500, bottom: VIEW, width: 500, height: VIEW, x: 0, y: 0, toJSON() {} } as DOMRect;
      }
      if (el.dataset?.anchor && el.parentElement) {
        const list = el.parentElement;
        const spacer = Array.from(list.children).find((c) => c.getAttribute("aria-hidden") === "true") as
          | HTMLElement
          | undefined;
        const pad = Number.parseFloat(spacer?.style.height ?? "0") || 0;
        let preceding = 0;
        for (const sibling of Array.from(list.querySelectorAll("[data-anchor]"))) {
          if (sibling === el) break;
          preceding += heights.get(sibling) ?? ROW;
        }
        const rowTop = pad + preceding - top;
        return { top: rowTop, left: 0, right: 500, bottom: rowTop + h, width: 500, height: h, x: 0, y: rowTop, toJSON() {} } as DOMRect;
      }
      return { top: 0, left: 0, right: 0, bottom: h, width: 0, height: h, x: 0, y: 0, toJSON() {} } as DOMRect;
    });
    class GeoRO {
      private readonly cb: () => void;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(el: Element) {
        observerCbs.set(el, this.cb);
      }
      unobserve(el: Element) {
        observerCbs.delete(el);
      }
      disconnect() {
        observerCbs.clear();
      }
    }
    vi.stubGlobal("ResizeObserver", GeoRO);
    const scroller = () => screen.getByTestId("transcript-scroller") as HTMLElement;
    const defineScroll = () => {
      const el = scroller();
      Object.defineProperty(el, "scrollTop", {
        configurable: true,
        get: () => top,
        set: (v: number) => {
          const changed = v !== top;
          top = v;
          opts.onWrite?.(v);
          if (echoOnWrite && changed) fireEvent.scroll(el);
        },
      });
    };
    const scrollTo = (value: number) => {
      top = value;
      fireEvent.scroll(scroller());
    };
    const growRow = (ordinal: number, height: number) => {
      const el = scroller().querySelectorAll<HTMLElement>("[data-anchor]")[ordinal];
      if (!el) throw new Error(`mounted row ${ordinal} not found`);
      heights.set(el, height);
      observerCbs.get(el)?.();
    };
    const setTotal = (n: number) => {
      dynamicTotal = n;
    };
    return { scroller: () => scroller(), defineScroll, scrollTo, growRow, top: () => top, setTotal };
  }

  /**
   * Turn-start fixture: a fold of tc-a/tc-b/tc-c mid-turn, the assistant end,
   * then 15 more message turns so the list is long enough to scroll inside.
   */
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("a route switch mid-click clears the restore so estimate convergence resumes (item 4)", async () => {
    const user = userEvent.setup();
    const geo = installGeo(60);
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    // Session A: a click whose read never returns.
    const aEvents: Observation[] = [];
    for (let i = 0; i < 40; i += 1) aEvents.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", "insA"));
    makeClient(
      reg,
      "insA",
      aEvents,
      () => new Promise(() => {}),
      "1001",
      "1040",
    );
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });
    // Session B: a 200-node follow-pinned window. Its first-paint head rows
    // report 96px and the pinned tail rows are later measured at 50px; middle
    // rows stay unmeasured and render at whatever the estimate converged to.
    const bEvents: Observation[] = [];
    for (let i = 0; i < 200; i += 1) bEvents.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", "insB"));
    makeClient(reg, "insB", bEvents, vi.fn<JournalRead>().mockResolvedValue(pageOf([])), "1", "1200");

    function GoB() {
      const navigate = useNavigate();
      return (
        <button type="button" data-testid="go-b" onClick={() => navigate("/s/insB")}>
          go
        </button>
      );
    }

    render(
      <MemoryRouter initialEntries={["/s/insA"]}>
        <Routes>
          <Route
            path="/s/:instanceId"
            element={
              <>
                <Driver reg={reg} compact={false} />
                <GoB />
              </>
            }
          />
        </Routes>
      </MemoryRouter>,
    );
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));

    // Switch routes while A's read is stuck: the epoch reset must clear A's
    // restore (a leaked restoringRef freezes the row-height estimate).
    await user.click(screen.getByTestId("go-b"));
    await act(async () => {});
    geo.setTotal(200);

    // Measure the pinned tail rows at 50px: with the route reset the estimate
    // converges; a leaked restoringRef from A keeps it frozen at 96px.
    for (let pass = 0; pass < 4; pass += 1) {
      const rows = geo.scroller().querySelectorAll<HTMLElement>("[data-anchor]");
      await act(async () => {
        rows.forEach((_i, idx) => {
          if (idx >= rows.length - 16) geo.growRow(idx, 200);
        });
      });
    }

    // Jump to node 25: head rows hold 96px mount measurements, middle rows are
    // unmeasured placeholders. Frozen estimate 96px -> 2400; a converged
    // estimate (< 96px) lands noticeably higher.
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "insB-m1026");
    await user.keyboard("[Enter]");
    // Route reset restored estimate convergence: the 200px measurements move
    // the jump well past the frozen-96 geometry (which lands near 4064).
    expect(geo.top()).toBeGreaterThan(4500);
  });

});
