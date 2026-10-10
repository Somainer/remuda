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

function thoughtEvent(seq: number, text = "thinking it over"): Observation {
  return obs(seq, "thought", {
    nodeId: `nt-${seq}` as Id,
    thoughtId: `th-${seq}` as Id,
    revision: "1",
    baseRevision: null,
    operation: "append",
    partIndex: 0,
    representation: "summary",
    status: "complete",
    text,
  });
}

describe("c-uifold compact process fold", () => {
  it("expands and collapses again; the caret flips and aria-expanded matches", async () => {
    const user = userEvent.setup();
    renderRouted([
      userMessage(1, "跑一下"),
      ...settledBash(2, 3),
      thoughtEvent(4),
      assistantMessage(5, "好了"),
    ]);
    const fold = () => screen.getByTestId("compact-fold");
    const wrap = () => screen.getByTestId("compact-fold-wrap");
    // Collapsed: right caret, count summary, no mounted body.
    expect(fold()).toHaveAttribute("aria-expanded", "false");
    expect(fold().textContent).toContain("▸");
    expect(fold().textContent).toContain("1 次工具 · 1 段思考");
    expect(wrap().querySelector("[data-testid='tool-card']")).toBeNull();

    // Open: down caret, 收起过程 label, body mounted.
    await user.click(fold());
    expect(fold()).toHaveAttribute("aria-expanded", "true");
    expect(fold().textContent).toContain("▾");
    expect(fold().textContent).toContain("收起过程");
    expect(wrap().querySelector("[data-testid='tool-card']")).not.toBeNull();

    // Clicking the SAME summary row collapses it again.
    await user.click(fold());
    expect(fold()).toHaveAttribute("aria-expanded", "false");
    expect(fold().textContent).toContain("▸");
    expect(wrap().querySelector("[data-testid='tool-card']")).toBeNull();
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
  function installGeometry(totalCount: number, opts: { echoOnWrite?: boolean } = {}) {
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
      private el: Element | null = null;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(el: Element) {
        this.el = el;
        observerCbs.set(el, this.cb);
      }
      unobserve(el: Element) {
        if (observerCbs.get(el) === this.cb) observerCbs.delete(el);
      }
      disconnect() {
        // Real ResizeObserver disconnects only THIS observation: unmounting a
        // row must not drop the surviving rows' callbacks from the map.
        if (this.el && observerCbs.get(this.el) === this.cb) observerCbs.delete(this.el);
        this.el = null;
      }
    }
    vi.stubGlobal("ResizeObserver", GeometryRO);

    const scroller = () => screen.getByTestId("transcript-scroller") as HTMLElement;
    // Browser-like scroll model: the value clamps to [0, scrollHeight -
    // clientHeight], writing the same value dispatches NO event, and a
    // programmatic write's scroll event is delivered on the next animation
    // frame (coalesced: multiple writes in one frame fire once).
    let queued = false;
    const maxScroll = () => Math.max(0, listHeight() - VIEW);
    const dispatchScroll = () => {
      queued = false;
      fireEvent.scroll(scroller());
    };
    const defineScroll = () => {
      const el = scroller();
      Object.defineProperty(el, "scrollTop", {
        configurable: true,
        get: () => top,
        set: (v: number) => {
          const clamped = Math.max(0, Math.min(v, maxScroll()));
          if (clamped === top) return;
          top = clamped;
          // Simulate a browser delivering the programmatic scroll's own event
          // queued on the next frame (real scroll events are coalesced, not
          // microtask-ordered against the click's finally).
          if (opts.echoOnWrite && !queued) {
            queued = true;
            requestAnimationFrame(dispatchScroll);
          }
        },
      });
    };
    // An explicit reader gesture: clamped like the browser and dispatched
    // synchronously by the test.
    const scrollTo = (value: number) => {
      top = Math.max(0, Math.min(value, maxScroll()));
      fireEvent.scroll(scroller());
    };
    // A real WHEEL gesture: the browser scrolls ~delta then fires wheel (and
    // later a coalesced scroll). Clamp like the browser; tiny deltas still
    // move within rounding.
    const wheel = (deltaY: number) => {
      top = Math.max(0, Math.min(top + deltaY, maxScroll()));
      fireEvent.wheel(scroller(), { deltaY });
    };
    /** Flush the setter's coalesced next-frame scroll event(s). */
    const nextFrame = () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
      });
    const growMountedRow = (ordinal: number, height: number) => {
      const el = scroller().querySelectorAll<HTMLElement>("[data-anchor]")[ordinal];
      if (!el) throw new Error(`mounted row ${ordinal} not found`);
      heights.set(el, height);
      observerCbs.get(el)?.();
    };
    const scrollTopNow = () => top;
    return { scroller, defineScroll, scrollTo, wheel, growMountedRow, scrollTopNow, nextFrame };
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

// ---------------------------------------------------------------------------
// UO-6a round 5: programmatic vs user scroll, post-prepend fold retarget,
// final-page settle, route-change restore reset, reconnect duplicate paging.
// ---------------------------------------------------------------------------
describe("load-earlier anchor lifecycle round 5", () => {
  const ROW = 96;
  const VIEW = 720;
  // Active browser-like scroll model: the prototype scrollTop override is
  // installed once but reads the CURRENT installGeo harness (its closures
  // differ per test), so each test's clamp/event/onWrite options apply.
  type ScrollModel = {
    echoOnWrite: boolean;
    onWrite?: (v: number) => void;
    clampHeight?: () => number;
    total: () => number;
    clientHeight: () => number;
    getTop: () => number;
    setTop: (v: number) => void;
    queue: () => void;
    isQueued: () => boolean;
    clearQueued: () => void;
  };
  let activeScroll: ScrollModel | null = null;
  let protoScrollInstalled = false;
  // Where jsdom defines the native accessor: restore by defineProperty on the
  // same object, or by deleting an own override that shadowed a prototype one.
  const nativeScrollOwner: { desc: PropertyDescriptor; on: Element } | null = (() => {
    const onH = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop");
    if (onH) return { desc: onH, on: HTMLElement.prototype };
    const onE = Object.getOwnPropertyDescriptor(Element.prototype, "scrollTop");
    return onE ? { desc: onE, on: Element.prototype } : null;
  })();
  const restoreNativeScrollTop = () => {
    if (!protoScrollInstalled || !nativeScrollOwner) return;
    if (nativeScrollOwner.on === HTMLElement.prototype) {
      Object.defineProperty(HTMLElement.prototype, "scrollTop", nativeScrollOwner.desc);
    } else {
      delete (HTMLElement.prototype as unknown as Record<string, unknown>).scrollTop;
    }
    protoScrollInstalled = false;
  };

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

  function toolAt(seq: number, tcId: string, instanceId: string): Observation[] {
    const s = source();
    return [
      {
        schemaVersion: 1,
        eventId: `e_${seq}` as Id,
        journalId: `obj_${instanceId}` as Id,
        instanceId: instanceId as Id,
        runId: null,
        hostId: "hst" as Id,
        processGeneration: "1",
        runGeneration: null,
        seq: String(seq),
        observedAt: "2026-09-12T00:00:00.000Z",
        nativeAt: known("2026-09-12T00:00:00.000Z"),
        source: s,
        kind: "tool_call",
        completeness: "structured",
        rawRef: null,
        evidenceEventIds: [],
        payload: {
          nodeId: `nc_${tcId}` as Id,
          revision: "1",
          operation: "open",
          baseRevision: null,
          toolCallId: tcId as Id,
          parentToolCallId: null,
          toolName: known("Bash"),
          displayTitle: known("Bash"),
          category: "shell",
          input: known({ command: tcId }),
          inputTextDelta: null,
          state: "running",
          executor: known({ hostId: "hst" as Id, workspaceId: null, nativeAgentId: null }),
        },
      } as Observation,
      {
        schemaVersion: 1,
        eventId: `e_${seq + 1}` as Id,
        journalId: `obj_${instanceId}` as Id,
        instanceId: instanceId as Id,
        runId: null,
        hostId: "hst" as Id,
        processGeneration: "1",
        runGeneration: null,
        seq: String(seq + 1),
        observedAt: "2026-09-12T00:00:00.000Z",
        nativeAt: known("2026-09-12T00:00:00.000Z"),
        source: s,
        kind: "tool_result",
        completeness: "structured",
        rawRef: null,
        evidenceEventIds: [],
        payload: {
          nodeId: `nr_${tcId}` as Id,
          revision: "1",
          operation: "close",
          baseRevision: null,
          toolCallId: tcId as Id,
          stage: "final",
          outcome: "succeeded",
          blocks: [{ type: "text", text: tcId }],
          structuredResult: unknownKnowledge("text"),
          exitCode: known(0),
          changes: [],
        },
      } as Observation,
    ];
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

  function gate<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => {
      resolve = r;
    });
    return { promise, resolve };
  }

  /** Geometry harness scoped to one Driver render (see round-4 block). */
  function installGeo(
    totalCount: number,
    opts: { echoOnWrite?: boolean; onWrite?: (v: number) => void; clampHeight?: () => number } = {},
  ) {
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
      private el: Element | null = null;
      constructor(cb: () => void) {
        this.cb = cb;
      }
      observe(el: Element) {
        this.el = el;
        observerCbs.set(el, this.cb);
      }
      unobserve(el: Element) {
        if (observerCbs.get(el) === this.cb) observerCbs.delete(el);
      }
      disconnect() {
        // Real ResizeObserver disconnects only THIS observation: unmounting a
        // row must not drop the surviving rows' callbacks from the map.
        if (this.el && observerCbs.get(this.el) === this.cb) observerCbs.delete(this.el);
        this.el = null;
      }
    }
    vi.stubGlobal("ResizeObserver", GeoRO);
    const scroller = () => screen.getByTestId("transcript-scroller") as HTMLElement;
    // Browser-like scroll model installed on the PROTOTYPE, so it also
    // intercepts the component's very first-mount restore write (an
    // element-level override installed after render misses it). Clamp to
    // [0, scrollHeight - clientHeight], no event when the value does not
    // change, and a programmatic write's scroll event is delivered next frame
    // (coalesced). Non-scroller elements keep the native accessor.
    let queued = false;
    const model: ScrollModel = {
      echoOnWrite,
      onWrite: opts.onWrite,
      clampHeight: opts.clampHeight,
      total: () => dynamicTotal,
      clientHeight: () => VIEW,
      getTop: () => top,
      setTop: (v) => {
        top = v;
      },
      queue: () => {
        queued = true;
      },
      isQueued: () => queued,
      clearQueued: () => {
        queued = false;
      },
    };
    activeScroll = model;
    const nativeDesc = nativeScrollOwner?.desc;
    if (!protoScrollInstalled) {
      Object.defineProperty(HTMLElement.prototype, "scrollTop", {
        configurable: true,
        get(this: HTMLElement) {
          if (isScroller(this) && activeScroll) return activeScroll.getTop();
          return nativeDesc?.get?.call(this) as number;
        },
        set(this: HTMLElement, v: number) {
          const m = activeScroll;
          if (!isScroller(this) || !m) {
            nativeDesc?.set?.call(this, v);
            return;
          }
          const max = Math.max(0, (m.clampHeight?.() ?? m.total() * ROW) - m.clientHeight());
          const clamped = Math.max(0, Math.min(v, max));
          if (clamped === m.getTop()) return;
          m.setTop(clamped);
          m.onWrite?.(clamped);
          if (m.echoOnWrite && !m.isQueued()) {
            m.queue();
            const el = this;
            requestAnimationFrame(() => {
              m.clearQueued();
              fireEvent.scroll(el);
            });
          }
        },
      });
      protoScrollInstalled = true;
    }
    // Kept for call-site compatibility: the prototype model needs no element
    // setup and works before render.
    const defineScroll = () => {};
    // An explicit reader gesture: clamped and dispatched synchronously.
    const scrollTo = (value: number) => {
      const max = Math.max(0, (opts.clampHeight?.() ?? dynamicTotal * ROW) - VIEW);
      top = Math.max(0, Math.min(value, max));
      fireEvent.scroll(scroller());
    };
    // A real wheel gesture: scroll ~delta (clamped) then fire wheel.
    const wheel = (deltaY: number) => {
      const max = Math.max(0, (opts.clampHeight?.() ?? dynamicTotal * ROW) - VIEW);
      top = Math.max(0, Math.min(top + deltaY, max));
      fireEvent.wheel(scroller(), { deltaY });
    };
    /** Flush the setter's coalesced next-frame scroll event(s). */
    const nextFrame = () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
      });
    const growRow = (ordinal: number, height: number) => {
      const el = scroller().querySelectorAll<HTMLElement>("[data-anchor]")[ordinal];
      if (!el) throw new Error(`mounted row ${ordinal} not found`);
      heights.set(el, height);
      observerCbs.get(el)?.();
    };
    const setTotal = (n: number) => {
      dynamicTotal = n;
    };
    return { scroller: () => scroller(), defineScroll, scrollTo, wheel, growRow, top: () => top, setTotal, nextFrame };
  }

  /**
   * Turn-start fixture: a fold of tc-a/tc-b/tc-c mid-turn, the assistant end,
   * then 15 more message turns so the list is long enough to scroll inside.
   */
  function foldSession(): Observation[] {
    const initial: Observation[] = [
      ...toolAt(201, "r5-tc-a", "insX"),
      ...toolAt(203, "r5-tc-b", "insX"),
      ...toolAt(205, "r5-tc-c", "insX"),
      m(207, "assistant", "insX"),
    ];
    for (let seq = 208, i = 0; i < 15; i += 1, seq += 2) {
      initial.push(m(seq, "user", "insX"));
      initial.push(m(seq + 1, "assistant", "insX"));
    }
    return initial;
  }

  /** Same-turn older tools that rename the fold to compact:r5-tc-x. */
  function sameTurnOlderTools(): Observation[] {
    return [...toolAt(106, "r5-tc-x", "insX"), ...toolAt(108, "r5-tc-y", "insX")];
  }

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    activeScroll = null;
    // Restore the native scrollTop accessor the geometry stub replaced.
    restoreNativeScrollTop();
  });

  it("a restore whose own scroll event lands mid-flight completes instead of cancelling (item 1)", async () => {
    const user = userEvent.setup();
    const geo = installGeo(43, { echoOnWrite: true });
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    const g = gate<ReturnType<typeof pageOf>>();
    const done = gate<void>();
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      // The client emits (and the restore writes + its scroll event fires)
      // BEFORE the click's finally settles: hold the Transcript continuation
      // on `done` so the echo truly lands while the request is in flight.
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      await done.promise;
      return result;
    });
    // Older page: 31 standalone rows + two earlier tools renaming the fold.
    const older: Observation[] = [];
    let seq = 101;
    for (let t = 0; t < 15; t += 1) {
      older.push(m(seq, "user", "insX"));
      older.push(m(seq + 1, "assistant", "insX"));
      seq += 2;
    }
    older.push(m(seq, "user", "insX"));
    older.push(...toolAt(seq + 1, "r5-tc-x", "insX"));
    older.push(...toolAt(seq + 3, "r5-tc-y", "insX"));
    makeClient(reg, "insX", foldSession(), () => g.promise, "201", "237");

    render(
      <MemoryRouter initialEntries={["/s/insX"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
        </Routes>
      </MemoryRouter>,
    );
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));

    // The client emits: the restore jumps to the fold 31 rows deep and the
    // browser echoes that programmatic scroll while the request is still in
    // flight. The matching-target guard must not cancel the restore.
    g.resolve(pageOf(older));
    await act(async () => {
      await Promise.resolve();
    });
    // Deliver the restore write's coalesced next-frame echo while the click
    // continuation is still held open on `done` — the event genuinely lands
    // mid-flight.
    await geo.nextFrame();
    expect(geo.top()).toBe(31 * ROW);

    // Mounted rows at the target window measure a very different real height
    // (50px). An armed restore FREEZES the unmeasured-row estimate at 96, so
    // the fold stays pinned at 2976; a wrongly self-cancelled restore clears
    // restoringRef, the estimate converges, and the anchor drifts to 31*50.
    for (let i = 0; i < 8; i += 1) geo.growRow(i, 50);
    expect(geo.top()).toBe(31 * ROW);

    // Now the finally settles; the button returns to idle.
    await act(async () => {
      done.resolve();
      await Promise.resolve();
    });
    expect((screen.getByTestId("load-earlier") as HTMLButtonElement).disabled).toBe(false);
  });

  it("retargets a renamed fold past newly inserted standalone rows to the content row (item 2)", async () => {
    const user = userEvent.setup();
    const geo = installGeo(43);
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });

    // Older page: a finished PREVIOUS turn (its lone tool stays standalone,
    // only one routine tool so no fold), then this turn's user message and two
    // earlier tools that rename the fold. The fold lands FOUR slots down.
    const older: Observation[] = [
      m(101, "user", "insX"),
      ...toolAt(102, "r5-tc-solo", "insX"),
      m(104, "assistant", "insX"),
      m(105, "user", "insX"),
      ...sameTurnOlderTools(),
    ];
    let resolve!: (v: ReturnType<typeof pageOf>) => void;
    makeClient(
      reg,
      "insX",
      foldSession(),
      () =>
        new Promise((r) => {
          resolve = r;
        }),
      "201",
      "237",
    );

    render(
      <MemoryRouter initialEntries={["/s/insX"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
        </Routes>
      </MemoryRouter>,
    );
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));
    await act(async () => {
      resolve(pageOf(older));
      await Promise.resolve();
    });

    // The restored anchor is the RENAMED FOLD, not the standalone user row at
    // the raw pre-insert index: the estimate jump is four rows deep.
    expect(geo.top()).toBe(4 * ROW);
    const fold = geo.scroller().querySelector("[data-anchor='compact:r5-tc-x']");
    expect(fold).toBeTruthy();
    expect(Math.round((fold as HTMLElement).getBoundingClientRect().top)).toBe(0);
  });

  it("keeps restoring on the final history page until the anchor settles (item 3)", async () => {
    const user = userEvent.setup();
    const jumps: number[] = [];
    const geo = installGeo(50, { onWrite: (v) => { if (v > 0) jumps.push(v); } });
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
    // 20-row tail; the final page adds 30 rows down to seq 1 (window complete),
    // so the armed first row lands at index 30, below the mounted window.
    const tail: Observation[] = [];
    for (let i = 0; i < 20; i += 1) tail.push(m(31 + i, i % 2 === 0 ? "user" : "assistant", "insE"));
    const finalPage: Observation[] = [];
    for (let seq = 1; seq <= 30; seq += 1) finalPage.push(m(seq, seq % 2 ? "user" : "assistant", "insE"));
    let resolve!: (v: ReturnType<typeof pageOf>) => void;
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const r = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return r;
    });
    makeClient(reg, "insE", tail, () => new Promise((r2) => { resolve = r2; }), "31", "50");
    render(<MemoryRouter initialEntries={["/s/insE"]}><Routes><Route path="/s/:instanceId" element={<Driver reg={reg} />} /></Routes></MemoryRouter>);
    geo.defineScroll();
    geo.scrollTo(0);
    await user.click(screen.getByTestId("load-earlier"));

    await act(async () => {
      resolve(pageOf(finalPage, true));
      await Promise.resolve();
    });
    jumps.length = 0;

    // Mount measurements keep arriving while the anchor is still deep and
    // unmounted: a live restore RE-JUMPS toward it on every measurement commit
    // until it mounts. The old end-page finally retired the restore, so the
    // jump happened once and never repeats.
    for (let i = 0; i <= 15; i += 1) {
      await act(async () => {
        geo.growRow(i, 50);
      });
    }
    expect(jumps.length).toBeGreaterThan(1);
    expect(screen.queryByTestId("load-earlier")).toBeNull();
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

  it("duplicate pages after a reconnect keep paging through until new rows prepend (item 5)", async () => {
    const user = userEvent.setup();
    const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };

    // Real bounded server: newest `window` rows of (afterSeq, beforeSeq].
    const window = 500;
    const read: JournalRead = vi.fn(async (args) => {
      const total = 3000;
      const after = Number(args.afterSeq ?? 0);
      const before = args.beforeSeq === undefined ? total : Math.min(Number(args.beforeSeq), total);
      const picked: number[] = [];
      for (let seq = before; seq > after && picked.length < window; seq -= 1) picked.push(seq);
      picked.reverse();
      const events = picked.map((seq) => m(seq, seq % 2 ? "user" : "assistant", "insR"));
      return pageOf(
        events,
        picked.length === 0 || picked[0] === after + 1,
      );
    });

    // Seed tail 1501..2500 (500-row seed + 1000 live rows applied later).
    const seed: Observation[] = [];
    for (let seq = 1501; seq <= 2000; seq += 1) seed.push(m(seq, seq % 2 ? "user" : "assistant", "insR"));
    const client = makeClient(reg, "insR", seed, read, "1501", "2000");
    // Live frames extend the applied cursor to 2500; mirror them into the
    // registry state the Transcript renders (the store does this onEvents).
    const live = Array.from({ length: 500 }, (_, i) => m(2001 + i, (2001 + i) % 2 ? "user" : "assistant", "insR"));
    client.applyBatch({
      subscriptionId: "sub",
      journalId: "obj_insR" as Id,
      fromSeq: "2001",
      toSeq: "2500",
      events: live,
      durableSeq: "2500",
    });
    reg.events.insR = seed.concat(live).sort((a, b) => Number(a.seq) - Number(b.seq));
    // Reconnect re-anchors the window floor above the loaded range.
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: "ep2" as Id,
      asOfSeq: "3000",
      instance: {} as Snapshot["instance"],
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "2501", complete: false },
    });
    vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
      const result = await reg.clients[instanceId]!.loadEarlier();
      reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
      return result;
    });

    render(
      <MemoryRouter initialEntries={["/s/insR"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
        </Routes>
      </MemoryRouter>,
    );

    const button = () => screen.queryByTestId("load-earlier");
    expect(button()).not.toBeNull();

    // Click 1: 2001..2500 all already held — duplicate, no prepend, still open.
    await user.click(button()!);
    expect(reg.events.insR).toHaveLength(1000);
    expect(button()).not.toBeNull();

    // Click 2: 1501..2000 is the seed — also a duplicate, still open.
    await user.click(button()!);
    expect(reg.events.insR).toHaveLength(1000);
    expect(button()).not.toBeNull();

    // Click 3: 1001..1500 is unseen — rows prepend through to the Transcript.
    await user.click(button()!);
    await act(async () => {});
    expect(reg.events.insR).toHaveLength(1500);
    expect(reg.events.insR[0]?.seq).toBe("1001");
    expect(button()).not.toBeNull();
  });

  // UO-6a round 6 item 2: while an older page is in flight, the restore must
  // suppress only its OWN scroll echo. j/k navigation, search jumps and 跳到最新
  // are intentional programmatic navigation: their events cancel the restore
  // and the landing prepend must not restore the click-time row instead.
  describe("round 6 item 2: in-flight programmatic navigation wins", () => {
    type OlderPage = ReturnType<typeof pageOf>;

    function setup(instanceId: string) {
      const user = userEvent.setup();
      const writes: number[] = [];
      const geo = installGeo(50, {
        echoOnWrite: true,
        onWrite: (v) => {
          writes.push(v);
        },
      });
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      const tail: Observation[] = [];
      for (let i = 0; i < 50; i += 1) tail.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", instanceId));
      const older: Observation[] = [];
      for (let seq = 901; seq <= 1000; seq += 1) {
        older.push(m(seq, seq % 2 ? "user" : "assistant", instanceId));
      }
      const g = gate<OlderPage>();
      makeClient(reg, instanceId, tail, () => g.promise, "1001", "1050");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (iid) => {
        const result = await reg.clients[iid]!.loadEarlier();
        reg.setFloor[iid]?.(reg.clients[iid]!.retainedFloorSeq);
        return result;
      });
      render(
        <MemoryRouter initialEntries={[`/s/${instanceId}`]}>
          <Routes>
            <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
          </Routes>
        </MemoryRouter>,
      );
      geo.defineScroll();
      geo.scrollTo(0);
      return { user, geo, g, writes, older };
    }

    async function land(geo: ReturnType<typeof setup>["geo"], g: ReturnType<typeof setup>["g"], older: Observation[]) {
      // Deliver the navigation's coalesced next-frame scroll event (its
      // cancellation) BEFORE the held page prepends.
      await geo.nextFrame();
      await act(async () => {
        g.resolve(pageOf(older));
        await Promise.resolve();
      });
      await act(async () => {});
    }

    // A leaked restore disables growth anchoring for the session (the hold
    // bails while a prepend anchor is held). Cancelling must release it: after
    // sitting at a genuine reading position, a row above the sampled anchor
    // grows and the scroller compensates.
    async function expectGrowthHolds(geo: ReturnType<typeof setup>["geo"]) {
      geo.scrollTo(25 * ROW);
      await act(async () => {});
      const before = geo.top();
      geo.growRow(4, ROW + 40);
      expect(geo.top()).toBeGreaterThanOrEqual(before + 39);
    }

    // r8 item 2: an in-flight navigation RETARGETS the load-earlier hold, so
    // after the prepend the DESTINATION row settles at the viewport top
    // (asserted on the row's rect, not scrollTop, which shifts with the
    // inserted rows).
    function nodeTop(geo: ReturnType<typeof setup>["geo"], nodeId: string): number | null {
      const row = geo.scroller().querySelector<HTMLElement>(`[data-anchor="${nodeId}"]`);
      if (!row) return null;
      return Math.round(row.getBoundingClientRect().top - geo.scroller().getBoundingClientRect().top);
    }

    it("j navigation during the fetch retargets the hold and lands the turn at the top", async () => {
      const { user, geo, g, writes, older } = setup("insJ");
      await user.click(screen.getByTestId("load-earlier"));
      // First press selects turn 0 (already at top: no write/no event); the
      // second jumps to turn 1 — the in-flight hold is re-aimed at turn 1.
      await user.keyboard("jj");
      await land(geo, g, older);
      // The click-time anchor would have restored to 100*ROW; the retargeted
      // hold never writes it. After the 100-row prepend the DESTINATION turn
      // (the second message, n_1002 assistant) sits at the viewport top.
      expect(writes).not.toContain(100 * ROW);
      expect(nodeTop(geo, "n_1002_assistant_insJ"), "the j turn did not land at the top after the prepend").toBe(0);
      expect((screen.getByTestId("load-earlier") as HTMLButtonElement).disabled).toBe(false);
      await expectGrowthHolds(geo);
    });

    it("跳到最新 during the fetch cancels the restore and keeps the tail pinned", async () => {
      const { user, geo, g, writes, older } = setup("insLatest");
      await user.click(screen.getByTestId("load-earlier"));
      await user.click(screen.getByTestId("jump-latest"));
      await land(geo, g, older);
      expect(writes).not.toContain(100 * ROW);
      // Pinned to the bottom: the browser clamps scrollTop to
      // scrollHeight - clientHeight, not scrollHeight.
      expect(geo.top()).toBe(50 * ROW - VIEW);
    });

    it("a search jump during the fetch cancels the restore and stays on its hit", async () => {
      const { user, geo, g, writes, older } = setup("insSearch");
      await user.click(screen.getByTestId("load-earlier"));
      await user.click(screen.getByTestId("transcript-search-open"));
      await user.type(screen.getByTestId("transcript-search-input"), "insSearch-m1035");
      await user.keyboard("[Enter]");
      await land(geo, g, older);
      // The prepend would restore the click-time row (100*ROW); the cancelled
      // restore never writes it and the scroller stays at the search target.
      expect(writes).not.toContain(100 * ROW);
      expect(geo.top()).toBe(34 * ROW);
      expect(geo.top()).not.toBe(100 * ROW);
      await expectGrowthHolds(geo);
    });

    // UO-6a round 7 item 1: cancellation must happen synchronously at the
    // navigation entry, not only when an onScroll event happens to fire. The
    // prepend here is shallow (5 rows) so the click-time anchor stays MOUNTED
    // and a still-armed restore visibly repins it; event timing is controlled
    // explicitly instead of flushed by land().
    describe("round 7 item 1: navigation cancels without relying on a scroll event", () => {
      type OlderPage = ReturnType<typeof pageOf>;
      function shallowSetup(instanceId: string) {
        const user = userEvent.setup();
        const writes: number[] = [];
        const geo = installGeo(20, { echoOnWrite: true, onWrite: (v) => writes.push(v) });
        const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
        const tail: Observation[] = [];
        for (let i = 0; i < 20; i += 1) tail.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", instanceId));
        // Shallow prepend: the armed anchor lands only 5 rows deep, still
        // inside the mounted viewport, so an armed restore repins it visibly.
        const older: Observation[] = [];
        for (let seq = 996; seq <= 1000; seq += 1) older.push(m(seq, seq % 2 ? "user" : "assistant", instanceId));
        const g = gate<OlderPage>();
        makeClient(reg, instanceId, tail, () => g.promise, "1001", "1020");
        vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (iid) => {
          const result = await reg.clients[iid]!.loadEarlier();
          reg.setFloor[iid]?.(reg.clients[iid]!.retainedFloorSeq);
          return result;
        });
        render(
          <MemoryRouter initialEntries={[`/s/${instanceId}`]}>
            <Routes>
              <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
            </Routes>
          </MemoryRouter>,
        );
        geo.defineScroll();
        geo.scrollTo(0);
        const resolvePage = async () => {
          await act(async () => {
            g.resolve(pageOf(older));
            await Promise.resolve();
          });
          await act(async () => {});
        };
        return { user, geo, g, writes, older, resolvePage };
      }

      it("a single j at the top (no scroll event) is not repinned by the prepend", async () => {
        const { user, geo, writes, resolvePage } = shallowSetup("insJZero");
        await user.click(screen.getByTestId("load-earlier"));
        // One j selects turn 0 already at the top: applyOffset writes an
        // UNCHANGED clamped value, so the browser dispatches NO event.
        await user.keyboard("j");
        // The shallow page lands with no queued event in between.
        await resolvePage();
        // The armed anchor sits 5 rows deep and is mounted: an uncancelled
        // restore repins it to 5*ROW; the cancelled restore leaves top at 0.
        expect(writes).not.toContain(5 * ROW);
        expect(geo.top()).toBe(0);
      });

      it("after jj (event delivered after the prepend) the estimate is not frozen", async () => {
        // Deep window: the armed anchor lands 100 rows deep (off-screen), so a
        // still-frozen restoringRef shows up only through the estimate an
        // off-window search uses — exactly bug (b).
        const { user, geo, g, writes, older } = setup("insJJEst");
        await user.click(screen.getByTestId("load-earlier"));
        // First j: unchanged write, no event. Second j: moves to turn 1, its
        // event stays queued for the next frame.
        await user.keyboard("jj");
        // The 100-row page prepends BEFORE the queued event fires; without the
        // synchronous cancel restoringRef would stay true forever.
        await act(async () => {
          g.resolve(pageOf(older));
          await Promise.resolve();
        });
        await act(async () => {});
        await geo.nextFrame();
        await act(async () => {});
        expect(writes).not.toContain(100 * ROW);
        geo.setTotal(150);

        // Measure the rows currently mounted near the top at 200px, with NO
        // navigation or gesture in between (the growth hold is suppressed by
        // the cancel). The estimate is global: frozen restoringRef keeps it
        // at 96; released, it converges toward 200.
        for (let pass = 0; pass < 4; pass += 1) {
          const rows = geo.scroller().querySelectorAll<HTMLElement>("[data-anchor]");
          await act(async () => {
            rows.forEach((_row, idx) => {
              if (idx < 16) geo.growRow(idx, 200);
            });
          });
        }
        // The first off-window navigation after the measurements: it must run
        // with restoringRef already false (synchronous cancel), so the hit
        // uses the converged estimate. Frozen at 96 the hit lands near 4064.
        await user.click(screen.getByTestId("transcript-search-open"));
        await user.type(screen.getByTestId("transcript-search-input"), "insJJEst-m1049");
        await user.keyboard("[Enter]");
        expect(geo.top()).toBeGreaterThan(4500);
      });

      it("a navigation whose event arrives after the prepend keeps its destination", async () => {
        const { user, geo, writes, resolvePage } = shallowSetup("insLateEvent");
        await user.click(screen.getByTestId("load-earlier"));
        await user.keyboard("jj");
        // Real response lands, THEN the deferred navigation scroll event.
        await resolvePage();
        await geo.nextFrame();
        await act(async () => {});
        // Without the synchronous cancel the coalesced post-prepend event
        // matches the restore's new echo and the prepend repins to 5*ROW.
        expect(writes).not.toContain(5 * ROW);
        expect(geo.top()).toBe(ROW);
      });
    });

    // UO-6a round 7 item 2: the browser can CLAMP a load-earlier restore
    // write when the tail below the armed anchor is shorter than the
    // viewport (the requested top exceeds scrollHeight - clientHeight). The
    // echo must record the kept value, not the requested one, or the restore
    // cancels its own clamped echo and releases the prepend repin.
    describe("round 7 item 2: a clamped restore echo does not self-cancel", () => {
      it("keeps the prepend anchor armed when the repin write is clamped mid-flight", async () => {
        const user = userEvent.setup();
        const writes: number[] = [];
        const TAIL = 7;
        const PREPEND = 40;
        const geo = installGeo(TAIL, {
          echoOnWrite: true,
          onWrite: (v) => writes.push(v),
        });
        const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
        const tail: Observation[] = [];
        for (let i = 0; i < TAIL; i += 1) tail.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", "insLEClamp"));
        const older: Observation[] = [];
        for (let seq = 961; seq <= 1000; seq += 1) older.push(m(seq, seq % 2 ? "user" : "assistant", "insLEClamp"));
        const g = gate<ReturnType<typeof pageOf>>();
        const done = gate<void>();
        makeClient(reg, "insLEClamp", tail, () => g.promise, "1001", "1007");
        vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (iid) => {
          const result = await reg.clients[iid]!.loadEarlier();
          reg.setFloor[iid]?.(reg.clients[iid]!.retainedFloorSeq);
          await done.promise;
          return result;
        });
        render(
          <MemoryRouter initialEntries={["/s/insLEClamp"]}>
            <Routes>
              <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
            </Routes>
          </MemoryRouter>,
        );
        geo.defineScroll();
        geo.scrollTo(0);
        await user.click(screen.getByTestId("load-earlier"));
        // The 40-row prepend lands: the scrollHeight model must already cover
        // it when the restore writes.
        geo.setTotal(TAIL + PREPEND);
        await act(async () => {
          g.resolve(pageOf(older));
          await Promise.resolve();
        });
        // The anchor lands 40 rows deep: requested 40*ROW=3840, but the 47
        // rows of content clamp the write to 47*ROW - VIEW = 3792.
        const clampMax = (TAIL + PREPEND) * ROW - VIEW;
        await geo.nextFrame();
        expect(writes).toContain(clampMax);

        // The clamped echo lands mid-flight. Settle the click, then grow a
        // mounted row BELOW the armed anchor: the still-armed repin recomputes
        // and scrolls up to hold the anchor (writing again); a restore that
        // self-cancelled on the clamped echo released the repin and stays
        // silent (the pin re-write is the same clamped value, also a no-op).
        await act(async () => {
          done.resolve();
          await Promise.resolve();
        });
        const before = writes.length;
        // While pinned at the clamp line nothing can move scrollTop, so grow
        // the journal below (normal live append): the tail unclamps and a
        // still-armed repin realizes its outstanding delta toward 40*ROW. A
        // self-cancelled restore released the repin, so only the pin write to
        // the new bottom appears and 40*ROW is never written.
        for (let i = 0; i < 10; i += 1) {
          reg.events.insLEClamp!.push(m(2001 + i, i % 2 ? "user" : "assistant", "insLEClamp"));
        }
        geo.setTotal(TAIL + PREPEND + 10);
        reg.setEvents.insLEClamp(reg.events.insLEClamp!.slice());
        await act(async () => {});
        expect(writes.length, "the clamped echo self-cancelled the prepend repin").toBeGreaterThan(before);
        expect(writes).toContain(PREPEND * ROW);
      });
    });
  });

  // UO-6a round 6 item 1: the held post-prepend repin must end even when
  // measurement commits STOP before the stable-pass count — either after the
  // quiet window, or immediately on a genuine reader scroll — so it can never
  // re-jump and undo a later scroll.
  describe("round 6 item 1: the held restore has a definite end", () => {
    type Page = ReturnType<typeof pageOf>;

    function fixture(instanceId: string) {
      const tail: Observation[] = [];
      for (let i = 0; i < 20; i += 1) tail.push(m(31 + i, i % 2 === 0 ? "user" : "assistant", instanceId));
      const older: Observation[] = [];
      for (let seq = 1; seq <= 30; seq += 1) older.push(m(seq, seq % 2 ? "user" : "assistant", instanceId));
      return { tail, older };
    }

    afterEach(() => {
      vi.useRealTimers();
    });

    it("releases after a quiet window and never undoes a later reader scroll", async () => {
      const user = userEvent.setup();
      const geo = installGeo(50);
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      const { tail, older } = fixture("insQuiet");
      const g = gate<Page>();
      makeClient(reg, "insQuiet", tail, () => g.promise, "31", "50");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
        const result = await reg.clients[instanceId]!.loadEarlier();
        reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
        return result;
      });
      render(
        <MemoryRouter initialEntries={["/s/insQuiet"]}>
          <Routes>
            <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
          </Routes>
        </MemoryRouter>,
      );
      geo.defineScroll();
      geo.scrollTo(0);
      await user.click(screen.getByTestId("load-earlier"));

      // Freeze before the prepend commits so the settle quiet timer is faked;
      // the page lands and corrections stop long before the stable passes.
      vi.useFakeTimers();
      await act(async () => {
        g.resolve(pageOf(older, true));
        await Promise.resolve();
      });
      act(() => {
        vi.advanceTimersByTime(850);
      });
      vi.useRealTimers();

      // Measurements stay quiet; the reader now scrolls and owns the
      // position. A restore that stayed armed would re-jump on the next
      // growth commit and disable growth anchoring (the probe below).
      geo.scrollTo(25 * ROW);
      await act(async () => {});
      const before = geo.top();
      geo.growRow(4, ROW + 40);
      expect(geo.top()).toBeGreaterThanOrEqual(before + 39);
    });

    // UO-6a round 7 item 4: the quiet-timer release must be observable on its
    // own, BEFORE any reader scroll. The earlier test only checked behaviour
    // AFTER geo.scrollTo, which releases the hold by itself — so it passed
    // even with the quiet release removed. Assert the hold is armed shortly
    // after the prepend and retired purely by crossing the 800ms quiet window.
    it("the quiet timer retires the hold with no reader scroll (data-prepend-hold)", async () => {
      const user = userEvent.setup();
      const geo = installGeo(50);
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      const { tail, older } = fixture("insQuietObservable");
      const g = gate<Page>();
      makeClient(reg, "insQuietObservable", tail, () => g.promise, "31", "50");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
        const result = await reg.clients[instanceId]!.loadEarlier();
        reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
        return result;
      });
      render(
        <MemoryRouter initialEntries={["/s/insQuietObservable"]}>
          <Routes>
            <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
          </Routes>
        </MemoryRouter>,
      );
      const hold = () => screen.getByTestId("transcript-scroller").getAttribute("data-prepend-hold");
      geo.defineScroll();
      geo.scrollTo(0);
      await user.click(screen.getByTestId("load-earlier"));

      vi.useFakeTimers();
      await act(async () => {
        g.resolve(pageOf(older, true));
        await Promise.resolve();
      });
      // Measurement corrections have stopped but the quiet window (800ms) has
      // not elapsed: the hold must still be ARMED. (The hold is what the
      // release is about to retire.)
      act(() => {
        vi.advanceTimersByTime(100);
      });
      expect(hold(), "the load-earlier hold is armed before the quiet window").toBe("1");

      // Cross the quiet window with NO reader scroll and no further size
      // commit: the timer alone must retire the hold and reflect it at once.
      act(() => {
        vi.advanceTimersByTime(750);
      });
      expect(hold(), "the quiet window retires the hold before any scroll").toBe("0");
      vi.useRealTimers();
      await act(async () => {});
      expect(hold(), "the retired hold stays retired").toBe("0");
    });

    it("a genuine reader scroll releases the settling restore immediately", async () => {
      const user = userEvent.setup();
      const geo = installGeo(50);
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      const { tail, older } = fixture("insGesture");
      const g = gate<Page>();
      makeClient(reg, "insGesture", tail, () => g.promise, "31", "50");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
        const result = await reg.clients[instanceId]!.loadEarlier();
        reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
        return result;
      });
      render(
        <MemoryRouter initialEntries={["/s/insGesture"]}>
          <Routes>
            <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
          </Routes>
        </MemoryRouter>,
      );
      geo.defineScroll();
      geo.scrollTo(0);
      await user.click(screen.getByTestId("load-earlier"));
      await act(async () => {
        g.resolve(pageOf(older, true));
        await Promise.resolve();
      });

      // The page landed (request done) but the stable passes have not
      // accumulated: a genuine gesture must release the hold immediately.
      geo.scrollTo(25 * ROW);
      await act(async () => {});
      const before = geo.top();
      geo.growRow(4, ROW + 40);
      expect(geo.top()).toBeGreaterThanOrEqual(before + 39);
    });

    // UO-6a round 7 item 3: a tiny WHEEL gesture (1.5px) cancels an armed
    // restore even though a scroll event that small can fall inside the
    // restore's ±2px echo window. Without an explicit gesture listener the
    // settle correction would treat the move as its own echo (or reverse it).
    it("a 1.5px wheel gesture while a restore is armed retires it", async () => {
      const user = userEvent.setup();
      const geo = installGeo(50);
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      const { tail, older } = fixture("insWheel");
      const g = gate<Page>();
      makeClient(reg, "insWheel", tail, () => g.promise, "31", "50");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
        const result = await reg.clients[instanceId]!.loadEarlier();
        reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
        return result;
      });
      render(
        <MemoryRouter initialEntries={["/s/insWheel"]}>
          <Routes>
            <Route path="/s/:instanceId" element={<Driver reg={reg} />} />
          </Routes>
        </MemoryRouter>,
      );
      geo.defineScroll();
      geo.scrollTo(0);
      await user.click(screen.getByTestId("load-earlier"));
      await act(async () => {
        g.resolve(pageOf(older, true));
        await Promise.resolve();
      });
      await act(async () => {});
      // Restore is armed (page landed but stable passes incomplete). A tiny
      // 1.5px wheel BEFORE any scroll event retires it.
      act(() => {
        geo.wheel(1.5);
      });
      await act(async () => {});
      // The reader now owns ~the post-wheel position. Pump a settle correction
      // frame and more size commits: a still-armed restore would re-run its
      // correction loop and pull the position back to the click-time anchor;
      // retired, the position stays at the reader's wheel spot (within the
      // 1.5px gesture).
      const afterWheel = geo.top();
      geo.growRow(4, ROW + 40);
      await act(async () => {});
      geo.growRow(5, ROW + 40);
      await act(async () => {});
      await geo.nextFrame();
      expect(Math.abs(geo.top() - afterWheel)).toBeLessThanOrEqual(8);
    });
  });

  // UO-6a round 6 item 3: a route switch must reset EVERY restore field, not
  // just the estimate freeze — the settle timer, the loading latch, the held
  // anchor/pending restore and the request identity.
  describe("round 6 item 3: route switch resets every restore field", () => {
    afterEach(() => {
      vi.useRealTimers();
    });

    it("clears the armed settle timer and all anchors when switching mid-settle", async () => {
      const user = userEvent.setup();
      const writes: number[] = [];
      const geo = installGeo(50, { onWrite: (v) => writes.push(v) });
      const reg: Registry = { events: {}, floors: {}, clients: {}, setEvents: {}, setFloor: {} };
      // A: a 20-row tail plus a final 30-row page; after the prepend the armed
      // anchor sits 30 rows deep, unmounted — only the settle timer holds it.
      const aTail: Observation[] = [];
      for (let i = 0; i < 20; i += 1) aTail.push(m(31 + i, i % 2 === 0 ? "user" : "assistant", "insResetA"));
      const aOlder: Observation[] = [];
      for (let seq = 1; seq <= 30; seq += 1) aOlder.push(m(seq, seq % 2 ? "user" : "assistant", "insResetA"));
      const gA = gate<ReturnType<typeof pageOf>>();
      // The store call stays UNFINISHED after the client already emitted the
      // prepend: the route switch happens with the click genuinely in flight
      // (loading latch + request identity held), not in its finally.
      const returnGate = gate<void>();
      makeClient(reg, "insResetA", aTail, () => gA.promise, "31", "50");
      // B: a 200-node window, floor above 1 so it shows its own pager.
      const bEvents: Observation[] = [];
      for (let i = 0; i < 200; i += 1) bEvents.push(m(1001 + i, i % 2 === 0 ? "user" : "assistant", "insResetB"));
      makeClient(reg, "insResetB", bEvents, vi.fn<JournalRead>().mockResolvedValue(pageOf([])), "1001", "1200");
      vi.spyOn(hubStore, "loadEarlier").mockImplementation(async (instanceId) => {
        const result = await reg.clients[instanceId]!.loadEarlier();
        reg.setFloor[instanceId]?.(reg.clients[instanceId]!.retainedFloorSeq);
        if (instanceId === "insResetA") await returnGate.promise;
        return result;
      });

      function GoB() {
        const navigate = useNavigate();
        return (
          <button type="button" data-testid="go-b" onClick={() => navigate("/s/insResetB")}>
            go
          </button>
        );
      }

      render(
        <MemoryRouter initialEntries={["/s/insResetA"]}>
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
      // Let the manual scroll's real-timer persist debounce flush so only the
      // restore's own timers remain once fake time starts.
      await act(async () => {
        await new Promise((r) => setTimeout(r, 300));
      });

      vi.useFakeTimers();
      await act(async () => {
        gA.resolve(pageOf(aOlder, true));
        await Promise.resolve();
      });
      // Fire the restore writes' rAFs (scheduled during the landing commit);
      // the 800ms settle timer is then the only pending timer.
      act(() => {
        vi.advanceTimersByTime(50);
      });
      expect(vi.getTimerCount()).toBe(1);
      writes.length = 0;

      // Switch routes WHILE A's restore is settling: the render-phase reset
      // must clear the settle timer and every restore ref.
      geo.setTotal(200);
      act(() => {
        fireEvent.click(screen.getByTestId("go-b"));
      });
      act(() => {
        vi.advanceTimersByTime(50);
      });
      expect(vi.getTimerCount()).toBe(0);
      // loadingEarlier reset: B's own pager is enabled, not stuck on A's click.
      expect((screen.getByTestId("load-earlier") as HTMLButtonElement).disabled).toBe(false);
      // pinRef reset: a fresh B session pins to the tail, which writes the
      // mount scrollHeight (clamped to scrollHeight - clientHeight) right
      // here — a leak suppresses the pin write and the cleared log is empty.
      expect(writes).toContain(200 * ROW - VIEW);

      // Past A's settle deadline: no late correction toward A's anchor (30
      // rows deep) may land on B, and no timer is ever re-armed.
      act(() => {
        vi.advanceTimersByTime(2000);
      });
      expect(vi.getTimerCount()).toBe(0);
      expect(writes).not.toContain(30 * ROW);

      // The rest mirrors real timing: restoringRef reset lets B's estimate
      // converge, and the anchor reset leaves growth anchoring working.
      vi.useRealTimers();

      // A's click finally lands late: its request identity was superseded by
      // the route reset, so it must not finalize anything on B or restore A's
      // anchor (30 rows deep).
      await act(async () => {
        returnGate.resolve();
        await Promise.resolve();
      });
      expect((screen.getByTestId("load-earlier") as HTMLButtonElement).disabled).toBe(false);
      expect(writes).not.toContain(30 * ROW);

      // Estimate convergence (same geometry as the round-5 route test):
      // frozen at 96 the hit lands near 4064, converged past 4500.
      for (let pass = 0; pass < 4; pass += 1) {
        const rows = geo.scroller().querySelectorAll<HTMLElement>("[data-anchor]");
        await act(async () => {
          rows.forEach((_row, idx) => {
            if (idx >= rows.length - 16) geo.growRow(idx, 200);
          });
        });
      }
      await user.click(screen.getByTestId("transcript-search-open"));
      await user.type(screen.getByTestId("transcript-search-input"), "insResetB-m1026");
      await user.keyboard("[Enter]");
      expect(geo.top()).toBeGreaterThan(4500);

      // The geometry harness does not echo programmatic scrollTop writes into
      // a scroll event, so React's scrollTop state still renders the head:
      // re-dispatch the hit position to mount the hit rows, let the search
      // refinement clear, and THEN verify the reading anchor works — a leaked
      // prepend anchor makes its hold bail for the whole session.
      geo.scrollTo(geo.top());
      for (let flush = 0; flush < 5; flush += 1) {
        await act(async () => {});
      }
      geo.scrollTo(25 * ROW);
      await act(async () => {});
      const before = geo.top();
      // A mounted row above the sampled anchor grows: a leaked prepend anchor
      // makes the reading-anchor hold bail for the whole session, so the
      // scroller would stay put instead of compensating by ~40px.
      geo.growRow(4, ROW + 40);
      await act(async () => {});
      expect(geo.top()).toBeGreaterThanOrEqual(before + 39);
    });
  });
});
describe("font reflow compensator", () => {
  const ROW = 96;
  const VIEW = 720;
  type ScrollModel = {
    total: () => number;
    getTop: () => number;
    setTop: (v: number) => void;
  };
  let activeScroll: ScrollModel | null = null;
  let protoInstalled = false;
  const nativeOwner = (() => {
    const onH = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop");
    return onH ? { desc: onH, on: HTMLElement.prototype } : null;
  })();
  const restoreNative = () => {
    if (!protoInstalled || !nativeOwner) return;
    if (nativeOwner.on === HTMLElement.prototype) Object.defineProperty(HTMLElement.prototype, "scrollTop", nativeOwner.desc);
    else delete (HTMLElement.prototype as unknown as Record<string, unknown>).scrollTop;
    protoInstalled = false;
  };

  function installGeo(rowCount: number) {
    let dynamicTotal = rowCount;
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
        const spacer = Array.from(list.children).find((c) => c.getAttribute("aria-hidden") === "true") as HTMLElement | undefined;
        const pad = Number.parseFloat(spacer?.style.height ?? "0") || 0;
        let preceding = 0;
        for (const sib of Array.from(list.querySelectorAll("[data-anchor]"))) {
          if (sib === el) break;
          preceding += heights.get(sib) ?? ROW;
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
        if (observerCbs.get(el) === this.cb) observerCbs.delete(el);
      }
      disconnect() {
        for (const [el, cb] of observerCbs) if (cb === this.cb) observerCbs.delete(el);
      }
    }
    vi.stubGlobal("ResizeObserver", GeoRO);
    activeScroll = { total: () => dynamicTotal, getTop: () => top, setTop: (v) => (top = v) };
    if (!protoInstalled) {
      Object.defineProperty(HTMLElement.prototype, "scrollTop", {
        configurable: true,
        get(this: HTMLElement) {
          if (isScroller(this) && activeScroll) return activeScroll.getTop();
          return nativeOwner?.desc.get?.call(this) as number;
        },
        set(this: HTMLElement, v: number) {
          const m = activeScroll;
          if (!isScroller(this) || !m) {
            nativeOwner?.desc.set?.call(this, v);
            return;
          }
          // Browser model: clamp, no event when unchanged; a programmatic
          // write's scroll event is delivered next frame (coalesced).
          const max = Math.max(0, m.total() * ROW - VIEW);
          const clamped = Math.max(0, Math.min(v, max));
          if (clamped === m.getTop()) return;
          m.setTop(clamped);
          const el = this;
          requestAnimationFrame(() => fireEvent.scroll(el));
        },
      });
      protoInstalled = true;
    }
    const scroller = () => screen.getByTestId("transcript-scroller") as HTMLElement;
    const readerScroll = (value: number) => {
      const max = Math.max(0, dynamicTotal * ROW - VIEW);
      top = Math.max(0, Math.min(value, max));
      fireEvent.scroll(scroller());
    };
    const growById = (nodeId: string, height: number) => {
      const el = scroller().querySelector<HTMLElement>(`[data-anchor="${nodeId}"]`);
      if (!el) throw new Error(`row ${nodeId} not mounted`);
      heights.set(el, height);
      observerCbs.get(el)?.();
    };
    // Set a row's geometry WITHOUT delivering its ResizeObserver callback: a
    // real font swap resizes every mounted row in the SAME layout, then the
    // per-row observers deliver in observer-CREATION order. Preset the grown
    // heights first so geometry already reflects the reflow when the first
    // callback runs, then deliver callbacks in a chosen order.
    const presetHeight = (nodeId: string, height: number) => {
      const el = scroller().querySelector<HTMLElement>(`[data-anchor="${nodeId}"]`);
      if (!el) throw new Error(`row ${nodeId} not mounted`);
      heights.set(el, height);
    };
    const fireMeasure = (nodeId: string) => {
      const el = scroller().querySelector<HTMLElement>(`[data-anchor="${nodeId}"]`);
      if (!el) throw new Error(`row ${nodeId} not mounted`);
      observerCbs.get(el)?.();
    };
    const nextFrame = () =>
      new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
    // Set scrollTop WITHOUT dispatching a scroll event: models a geometry the
    // restore has not yet corrected (an estimate miss), with no reader input.
    const quietTop = (value: number) => {
      const max = Math.max(0, dynamicTotal * ROW - VIEW);
      top = Math.max(0, Math.min(value, max));
    };
    return {
      scroller,
      readerScroll,
      growById,
      presetHeight,
      fireMeasure,
      quietTop,
      top: () => top,
      nextFrame,
      setTotal: (n: number) => (dynamicTotal = n),
    };
  }

  /** Render a non-follow transcript restored to anchor node N (1-based) at 0 offset. */
  function renderRestored(instanceId: string, anchorN: number, count = 40) {
    localStorage.clear();
    const events = buildLongObservations({
      instanceId: instanceId as Id,
      journalId: `obj_${instanceId}` as Id,
      hostId: "hst_1" as Id,
      count,
    });
    localStorage.setItem(
      `runtime.reading.v1.${instanceId}`,
      JSON.stringify({ anchorId: `obj_long_n_${anchorN}`, offset: 0, ratio: 0, avgRow: ROW, follow: false }),
    );
    return renderRouted(events, `/s/${instanceId}`, false);
  }

  /** Flush until the saved restore has settled (attribute flips to 0). */
  async function settle(geo: ReturnType<typeof installGeo>, instanceId: string) {
    for (let i = 0; i < 30; i += 1) {
      await act(async () => {
        await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
        await Promise.resolve();
      });
      const active = screen.getByTestId("transcript-scroller").getAttribute("data-restore-active");
      if (active === "0") return;
    }
    throw new Error(`restore never settled for ${instanceId} (top=${geo.top()})`);
  }

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    activeScroll = null;
    restoreNative();
    localStorage.clear();
    // The held-probe test puts ?restoreProbe=1 on the real jsdom location.
    window.history.replaceState({}, "", "/");
  });

  it("item 8: a row above the restored anchor grows -> scrollTop += delta", async () => {
    const geo = installGeo(40);
    renderRestored("insAbove", 5);
    await settle(geo, "insAbove");
    const before = geo.top();
    await act(async () => {
      geo.growById("obj_long_n_2", ROW + 40);
    });
    expect(geo.top(), "compensates for an above-row growth").toBe(before + 40);
  });

  it("item 3 mid: above-row growth while the held-probe restore is still ARMED is compensated", async () => {
    // Hold the font (restore stays data-restore-active=1), converge on the
    // fallback face, then grow an above row exactly like the mid font swap.
    const geo = installGeo(40);
    const fakeFonts = {
      check: () => false,
      load: async () => [] as FontFace[],
      ready: Promise.resolve({} as FontFaceSet),
      status: "loaded" as FontFaceSet["status"],
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
      onloading: null,
      onloadingdone: null,
      onloadingerror: null,
    } as unknown as FontFaceSet;
    Object.defineProperty(document, "fonts", { configurable: true, get: () => fakeFonts });
    // restoreProbe reads window.location.search (not the in-memory router), so
    // put the query on the real jsdom location.
    window.history.replaceState({}, "", "/s/insMid?restoreProbe=1");
    (window as unknown as { __fontSwapRestoreProbeArmed?: boolean }).__fontSwapRestoreProbeArmed = true;
    const events = buildLongObservations({
      instanceId: "insMid" as Id,
      journalId: "obj_insMid" as Id,
      hostId: "hst_1" as Id,
      count: 40,
    });
    localStorage.setItem(
      "runtime.reading.v1.insMid",
      JSON.stringify({ anchorId: "obj_long_n_5", offset: 0, ratio: 0, avgRow: ROW, follow: false }),
    );
    render(
      <MemoryRouter initialEntries={["/s/insMid?restoreProbe=1"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    // Converge while held: restore stays active but the offset reaches target.
    for (let i = 0; i < 30; i += 1) {
      await act(async () => {
        await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
        await Promise.resolve();
      });
    }
    const active = screen.getByTestId("transcript-scroller").getAttribute("data-restore-active");
    expect(active, "the probe keeps the restore armed").toBe("1");
    const before = geo.top();
    await act(async () => {
      geo.growById("obj_long_n_2", ROW + 40);
    });
    expect(geo.top(), "an armed mid-restore compensates an above-row growth").toBe(before + 40);
  });

  it("item 3 mid (below-anchor growth): a mid-restore reflow must not finalize while the anchor is still off its saved offset", async () => {
    // The growing row is strictly ABOVE the held restore anchor and the anchor
    // is still 30px short of its saved offset when the font lands (the bounded
    // long-journal mid arm: saved anchor is a burst row below the wrap block).
    // The reflow counter-scroll holds the anchor across the above row's growth,
    // but that MUST NOT finalize the restore at the unfinished spot. The
    // pending offset correction has to run first; the restore finalizes only
    // once the anchor actually reaches its saved offset.
    //
    // Coverage note (r8): with the DOM-relative compensator the whole drift is
    // written by the reflow correction itself, so this test now lands on the
    // saved offset under BOTH the pre-e6dc324d finalize-on-reflowCorrected
    // guard and the current code — it asserts the end-to-end sequence (do not
    // finalize while >2px off; finish AT the offset) but is not a red-on-prefix
    // proof. The guard is defense-in-depth for a clamped/interleaved
    // correction; that specific sequence is not driven here (clamped reflow
    // writes themselves ARE reachable — see the r7-item-2 clamp test and
    // installGeo.setTotal — but not the combination of a held, clamped,
    // off-saved-offset reflow that only becomes reachable after the range
    // grows).
    const geo = installGeo(40);
    let fontReady = false;
    const fakeFonts = {
      check: () => fontReady,
      load: async () => [] as FontFace[],
      ready: Promise.resolve({} as FontFaceSet),
      status: "loaded" as FontFaceSet["status"],
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
      onloading: null,
      onloadingdone: null,
      onloadingerror: null,
    } as unknown as FontFaceSet;
    Object.defineProperty(document, "fonts", { configurable: true, get: () => fakeFonts });
    window.history.replaceState({}, "", "/s/insMidBelow?restoreProbe=1");
    (window as unknown as { __fontSwapRestoreProbeArmed?: boolean }).__fontSwapRestoreProbeArmed = true;
    const events = buildLongObservations({
      instanceId: "insMidBelow" as Id,
      journalId: "obj_insMidBelow" as Id,
      hostId: "hst_1" as Id,
      count: 40,
    });
    const SAVED_OFFSET = 60;
    localStorage.setItem(
      "runtime.reading.v1.insMidBelow",
      JSON.stringify({ anchorId: "obj_long_n_5", offset: SAVED_OFFSET, ratio: 0, avgRow: ROW, follow: false }),
    );
    render(
      <MemoryRouter initialEntries={["/s/insMidBelow?restoreProbe=1"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    const scrollerEl = () => screen.getByTestId("transcript-scroller");
    const active = () => scrollerEl().getAttribute("data-restore-active");
    const anchorOffset = () => {
      const row = scrollerEl().querySelector<HTMLElement>('[data-anchor="obj_long_n_5"]');
      if (!row) return NaN;
      return row.getBoundingClientRect().top - scrollerEl().getBoundingClientRect().top;
    };
    const tick = () =>
      act(async () => {
        await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
        await Promise.resolve();
      });
    // Converge on the fallback face while the probe font stays held: the
    // anchor reaches the saved offset but the restore stays armed.
    for (let i = 0; i < 30; i += 1) await tick();
    expect(active(), "the probe keeps the restore armed before release").toBe("1");
    expect(Math.abs(anchorOffset() - SAVED_OFFSET)).toBeLessThanOrEqual(2);
    // Release the held font at the same moment the anchor is 30px BELOW its
    // saved offset (an estimate miss) and a row strictly above it grows.
    fontReady = true;
    await act(async () => {
      geo.quietTop(geo.top() - 30);
      expect(anchorOffset() - SAVED_OFFSET).toBeGreaterThan(2);
      geo.growById("obj_long_n_2", ROW + 40);
    });
    // The restore may not finalize while the anchor is still off: it is either
    // still armed (correcting) or has already reached the saved offset.
    expect(
      active() === "1" || Math.abs(anchorOffset() - SAVED_OFFSET) <= 2,
      "the reflow correction finalized the restore at an unfinished offset",
    ).toBe(true);
    // It then runs the pending offset correction and finalizes at the saved
    // offset. Under the old ledger-delta compensator the same sequence could
    // finalize ~30px short; at the tip (89623ff4 DOM-relative compensator)
    // both branches land at SAVED_OFFSET — see the coverage note above.
    for (let i = 0; i < 30; i += 1) await tick();
    expect(active(), "the restore finalizes once the saved offset is reached").toBe("0");
    expect(
      Math.abs(anchorOffset() - SAVED_OFFSET),
      "mid-restore reflow finalized away from the saved offset",
    ).toBeLessThanOrEqual(2);
  });
  it("item 1: the restored row ITSELF grows -> its own top is held, no scroll jump", async () => {
    const geo = installGeo(40);
    renderRestored("insSelf", 5);
    await settle(geo, "insSelf");
    const before = geo.top();
    await act(async () => {
      geo.growById("obj_long_n_5", ROW + 40);
    });
    expect(geo.top(), "the saved row's own growth must not scroll by its delta").toBe(before);
  });

  /** One swap resizes an above and a below row together; observers then fire. */
  async function twoRowSwapCompensatesOnce(
    instanceId: string,
    order: "below-first" | "above-first",
  ) {
    const geo = installGeo(40);
    renderRestored(instanceId, 5);
    await settle(geo, instanceId);
    const before = geo.top();
    const anchorOffset = () => {
      const row = geo
        .scroller()
        .querySelector<HTMLElement>('[data-anchor="obj_long_n_5"]');
      return row ? row.getBoundingClientRect().top - geo.scroller().getBoundingClientRect().top : NaN;
    };
    expect(Math.abs(anchorOffset())).toBeLessThanOrEqual(2);
    await act(async () => {
      // The swap resizes BOTH rows in a single layout before any observer
      // callback is delivered, so geometry already reflects the above row's
      // growth when the first callback runs.
      geo.presetHeight("obj_long_n_2", ROW + 40); // strictly ABOVE the anchor
      geo.presetHeight("obj_long_n_6", ROW + 30); // BELOW the anchor
      if (order === "below-first") {
        geo.fireMeasure("obj_long_n_6");
        geo.fireMeasure("obj_long_n_2");
      } else {
        geo.fireMeasure("obj_long_n_2");
        geo.fireMeasure("obj_long_n_6");
      }
    });
    await act(async () => {
      await geo.nextFrame();
    });
    // The above row's +40 is compensated exactly ONCE regardless of order and
    // the anchor's viewport spot is held. Note (r8): at the tip the below-row
    // observer never scrolls (the reflow anchor corrects the strictly-above
    // row only), so the below-first ordering is redundant by design — the
    // pre-r6 "before + 80" double count can no longer occur in either order
    // after the r7 item-3 DOM-relative fix. This gate is retained as the
    // order-independent anchor-stability regression check.
    expect(geo.top(), `two-row swap scrolled twice (order ${order})`).toBe(before + 40);
    expect(Math.abs(anchorOffset()), `anchor drifted (order ${order})`).toBeLessThanOrEqual(2);
  }

  it("item 2: two rows reflow, below-row observer first -> the drift is corrected once", async () => {
    await twoRowSwapCompensatesOnce("insOrderBelowFirst", "below-first");
  });

  it("item 2: two rows reflow, above-row observer first -> the drift is corrected once", async () => {
    await twoRowSwapCompensatesOnce("insOrderAboveFirst", "above-first");
  });

  it("item 2: after the reader scrolls up, a row between the new and old anchor does not jump", async () => {
    const geo = installGeo(40);
    renderRestored("insScroll", 5);
    await settle(geo, "insScroll");
    const restoredTop = geo.top();
    // Reader scrolls UP: the new reading anchor is a row near the viewport top
    // (n_2), while the saved row (n_5) is lower. Row n_3 sits BETWEEN them.
    act(() => geo.readerScroll(restoredTop - 200));
    await act(async () => {
      await geo.nextFrame();
    });
    const afterReader = geo.top();
    expect(afterReader).toBe(restoredTop - 200);
    // n_3 is BELOW the new reading anchor, so generic anchoring ignores its
    // growth. The sticky-reflow bug kept n_5 as the reflow anchor; n_3 is
    // strictly above it, so selfDelta jumped by the whole growth and rewrote
    // the reading anchor back to the saved row.
    await act(async () => {
      geo.growById("obj_long_n_3", ROW + 40);
    });
    expect(geo.top(), "a reader who navigated owns the position; no reflow jump").toBe(afterReader);
  });

  it("item 8: a sub-pixel measurement is a no-op", async () => {
    const geo = installGeo(40);
    renderRestored("insSub", 5);
    await settle(geo, "insSub");
    const before = geo.top();
    await act(async () => {
      geo.growById("obj_long_n_2", ROW + 0.5);
    });
    expect(geo.top()).toBe(before);
  });

  it("item 8: repeated same-height reports do not re-apply the delta", async () => {
    const geo = installGeo(40);
    renderRestored("insRepeat", 5);
    await settle(geo, "insRepeat");
    const before = geo.top();
    await act(async () => {
      geo.growById("obj_long_n_2", ROW + 40);
      geo.growById("obj_long_n_2", ROW + 40);
      geo.growById("obj_long_n_2", ROW + 40);
    });
    expect(geo.top(), "the committed delta is applied once").toBe(before + 40);
  });
  it("item 3: a measured height above the window survives fresh rows mounting on a window shift", async () => {
    // Coverage of the r6 ledger fix (5ad38bd7 "make the row-height ledger the
    // single source of truth"). The measured n_2 (above the shifted window)
    // keeps contributing its +40 to padTop across window shifts that mount
    // brand-new rows, whose mount-layout reports build on the ledger via
    // new Map(rowHeightsRef.current) — so this test covers the ledger-build
    // path by construction.
    //
    // It does NOT drive the exact stale-mirror interleaving described in
    // 5ad38bd7 (a passive state->ref mirror running with a stale closure
    // between a synchronous ledger write and its queued render). The window is
    // real in React 19 — the layout-effect setState schedules a SEPARATE
    // commit, and React flushes the prior commit's pending passive effects
    // before rendering it (exactly what 5ad38bd7 / Transcript.tsx guards
    // against) — but it could not be constructed in THIS harness: fresh rows
    // report ROW (identical to the estimate), so the sync re-render following
    // a window-shift ledger write mounts no further rows, and the
    // window-shift commit's own render changed no rowHeights, leaving no
    // mirror pending. Attempts: (a) an installGeo by-id mount-height preset
    // over scroll-up window shifts; (b) a scheduler/unstable_mock remount
    // (under which the saved-position restore cannot settle). Neither
    // reproduced the rollback; the guarded behaviour stays deleted rather
    // than re-proven red.
    const geo = installGeo(40);
    renderRestored("insLedger", 5);
    await settle(geo, "insLedger");
    await act(async () => {
      geo.growById("obj_long_n_2", ROW + 40);
    });
    const padTop = () => {
      const list = geo.scroller().firstElementChild;
      const spacer = Array.from(list?.children ?? []).find((c) => c.getAttribute("aria-hidden") === "true") as
        | HTMLElement
        | undefined;
      return Number.parseFloat(spacer?.style.height ?? "0") || 0;
    };
    // Expected padTop: `start` rows above the window at ROW each, plus n_2's
    // +40 (its index 1 is above any start >= 2), derived from the first mounted
    // row so it holds at any scroll position.
    const expectedPadTop = () => {
      const first = geo.scroller().querySelector<HTMLElement>("[data-anchor]");
      const k = Number(/obj_long_n_(\d+)/.exec(first?.dataset.anchor ?? "")?.[1] ?? 0);
      const start = k - 1;
      return start * ROW + (start >= 2 ? 40 : 0);
    };
    // Shift the window so n_2 is above it while fresh tail rows mount.
    act(() => geo.readerScroll(1300));
    await act(async () => {
      await geo.nextFrame();
    });
    expect(padTop(), "the measured above-window row's height was dropped from the ledger").toBe(expectedPadTop());
    // Further shifts that mount more fresh rows must still retain it.
    act(() => geo.readerScroll(1600));
    await act(async () => {
      await geo.nextFrame();
    });
    expect(padTop(), "the measured height was lost on a second window shift").toBe(expectedPadTop());
  });

  it("r7 item 2: a clamped no-op second internal write keeps the own-scroll latch armed", async () => {
    // Two internal writes land before the ONE coalesced scroll event: the
    // first moves (and arms reflowOwnScrollRef), the second is clamped at the
    // scroll max and moves nothing. An assign-style latch
    // (`latch = moved`) CLEARS it on the no-op, so the first write's event
    // then retires reflowAnchorRef as if the reader had scrolled. The latch
    // must arm only and stay armed; the anchor survives its own event.
    const geo = installGeo(40);
    const events = buildLongObservations({
      instanceId: "insLatch" as Id,
      journalId: "obj_insLatch" as Id,
      hostId: "hst_1" as Id,
      count: 40,
    });
    localStorage.clear();
    localStorage.setItem(
      "runtime.reading.v1.insLatch",
      // n_32 restored flush with the viewport top: target top 2976, 144px
      // below the clamp line (40*ROW - VIEW = 3120), far enough that
      // follow-pin (which arms within 64px of the bottom) does not grab it.
      JSON.stringify({ anchorId: "obj_long_n_32", offset: 0, ratio: 0, avgRow: ROW, follow: false }),
    );
    render(
      <MemoryRouter initialEntries={["/s/insLatch"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    await settle(geo, "insLatch");
    expect(geo.top()).toBe(2976);
    const hold = () => screen.getByTestId("transcript-scroller").getAttribute("data-reflow-hold");
    expect(hold()).toBe("1");
    await act(async () => {
      // ONE font layout resizes two mounted rows above the anchor before any
      // per-row observer delivers (geometry is preset for both; total drift
      // 248 pushes well past the 144px clamp margin). n_31 alone contributes a
      // +200 ledger delta — already more than the 144px margin — so the FIRST
      // delivery clamps under BOTH compensator forms (the old ledger-delta
      // form would add height - prevHeight = +200; the DOM-relative form
      // measures the full 248 drift, since both preset rows are already grown
      // in the DOM).
      geo.presetHeight("obj_long_n_30", ROW + 48);
      geo.presetHeight("obj_long_n_31", ROW + 200);
      // First delivery: the anchor drifted 248px in the DOM but the write
      // clamps at the scroll max (3120), moving only +144 and queueing the
      // coalesced event.
      geo.fireMeasure("obj_long_n_31");
      // Second delivery in the same synchronous batch (React has not flushed
      // the first write's state yet): the anchor is still 248-144 = 104px off
      // (48 of that is n_30's ledger delta under the old form), but the write
      // clamps to 3120 again — a true NO-OP in either compensator form, which
      // must not clear the latch the first write armed.
      geo.fireMeasure("obj_long_n_30");
    });
    // Deliver the queued event. With the old assign-on-write latch the event
    // finds the latch false and retires the anchor (data-reflow-hold flips to
    // "0" on the resulting render); arm-only keeps it held.
    await act(async () => {
      await geo.nextFrame();
    });
    expect(hold(), "the no-op second write disarmed the latch before the first write's own event").toBe("1");
    expect(geo.top()).toBe(3120);
  });

  it("r7 item 3: a streamed commit that already held the row is not double-counted by the row's RO delivery", async () => {
    // A React commit changing nodes/sizes (a streamed event) lands between the
    // font swap's LAYOUT (rows already resized in the DOM) and the per-row
    // ResizeObserver deliveries: with the resized row ABOVE the sampled
    // reading anchor, the commit's generic hold counter-scrolls the drift, and
    // row A's observer must not add its ledger height delta a SECOND time. The
    // explicit reflow correction is the anchor's DOM-relative drift, which is
    // 0 after the generic hold.
    const geo = installGeo(40);
    const opts = { instanceId: "insStream" as Id, journalId: "obj_insStream" as Id, hostId: "hst_1" as Id };
    localStorage.clear();
    localStorage.setItem(
      "runtime.reading.v1.insStream",
      // n_12 at saved offset 300 -> restore top 756; n_6 sits strictly ABOVE
      // the viewport then, so a later grow of n_6 drifts the on-screen anchor
      // and the sizes-commit generic hold compensates it.
      JSON.stringify({ anchorId: "obj_long_n_12", offset: 300, ratio: 0, avgRow: ROW, follow: false }),
    );
    // Stateful driver INSIDE the route (a rerendered MemoryRouter would not
    // propagate new props): bumping the count simulates a streamed nodes
    // commit exactly as SessionPage's event subscription would.
    let bump!: () => void;
    function Driver() {
      const [count, setCount] = useState(40);
      bump = () => setCount((n) => n + 1);
      return <Transcript events={buildLongObservations({ ...opts, count })} compact={false} />;
    }
    render(
      <MemoryRouter initialEntries={["/s/insStream"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Driver />} />
        </Routes>
      </MemoryRouter>,
    );
    await settle(geo, "insStream");
    const before = geo.top();
    expect(before).toBe(756);
    // The swap resizes n_6 (strictly above the anchor) in the DOM, then a
    // streamed nodes commit lands BEFORE n_6's observer delivery: the
    // commit's generic hold counter-scrolls the 40px drift.
    geo.presetHeight("obj_long_n_6", ROW + 40);
    await act(async () => {
      bump();
      await Promise.resolve();
    });
    expect(geo.top(), "the streamed commit holds the drifted anchor").toBe(before + 40);
    // The reflow anchor must still be armed at this point. Note the commit-level
    // generic hold is NOT gated on reflowAnchorRef, so it would still move the
    // +40 here with the anchor retired; the real risk is that n_6's later RO
    // delivery would then go through holdReadingAnchor instead of the reflow
    // compensator, and the double-count branch the test targets would never
    // run — the +40 expectation would pass without exercising the reflow path.
    expect(
      screen.getByTestId("transcript-scroller").getAttribute("data-reflow-hold"),
      "the reflow anchor retired before the row's RO delivery",
    ).toBe("1");
    // n_6's own observer delivers after that commit: the reflow anchor's DOM
    // drift is now 0 and the ledger height delta must not be added again.
    await act(async () => {
      geo.fireMeasure("obj_long_n_6");
      await geo.nextFrame();
    });
    expect(geo.top(), "the row's RO delivery double-counted the drift the streamed commit held").toBe(before + 40);
  });

  // UO-6a round 8 item 1: the reflow anchor armed by a restore lives only
  // until the reader moves. The clear must happen on the UNAMBIGUOUS input
  // paths even when they never dispatch a scroll event (a clamped wheel, a
  // programmatic navigation whose write is unchanged): otherwise a row growing
  // ABOVE the stale saved anchor later counter-scrolls the view even though
  // the reader already owns a different row.
  it("r8 item 1: a wheel gesture with no scroll event retires the restore's reflow anchor", async () => {
    const geo = installGeo(40);
    renderRestored("insWheelClear", 5);
    await settle(geo, "insWheelClear");
    const hold = () => screen.getByTestId("transcript-scroller").getAttribute("data-reflow-hold");
    expect(hold()).toBe("1");
    // A genuine wheel that the harness does NOT turn into a scroll event
    // (clamped / coalesced away in the browser): the gesture listener must
    // retire the anchor without waiting for onScroll.
    act(() => {
      fireEvent.wheel(geo.scroller(), { deltaY: 120 });
    });
    await act(async () => {
      await geo.nextFrame();
    });
    expect(hold(), "the wheel gesture left the restore reflow anchor armed").toBe("0");
  });

  it("r8 item 1: an unchanged programmatic navigation retires the anchor; a card between rows does not jump", async () => {
    // Restore n_35 to sit 192px below the viewport top (saved offset 192 -> top
    // 3264-192 = 3072 in a 60-row list): the topmost visible row is n_33, and
    // n_34 sits BETWEEN n_33 and the saved n_35. Growing n_34 must NOT scroll
    // — it is below the reader's topmost anchor.
    const geo = installGeo(60);
    const events = buildLongObservations({
      instanceId: "insNavClear" as Id,
      journalId: "obj_insNavClear" as Id,
      hostId: "hst_1" as Id,
      count: 60,
    });
    localStorage.clear();
    localStorage.setItem(
      "runtime.reading.v1.insNavClear",
      JSON.stringify({ anchorId: "obj_long_n_35", offset: 192, ratio: 0, avgRow: ROW, follow: false }),
    );
    const user = userEvent.setup();
    render(
      <MemoryRouter initialEntries={["/s/insNavClear"]}>
        <Routes>
          <Route path="/s/:instanceId" element={<Transcript events={events} compact={false} />} />
        </Routes>
      </MemoryRouter>,
    );
    await settle(geo, "insNavClear");
    expect(geo.top()).toBe(3072);
    const hold = () => screen.getByTestId("transcript-scroller").getAttribute("data-reflow-hold");
    expect(hold()).toBe("1");
    // Navigate to a hit that lands at the SAME scrollTop (n_33 base is exactly
    // 3072): the write is unchanged and dispatches no scroll event, so only the
    // synchronous non-restore clear retires the stale anchor.
    await user.click(screen.getByTestId("transcript-search-open"));
    await user.type(screen.getByTestId("transcript-search-input"), "prompt 33");
    await user.keyboard("[Enter]");
    expect(geo.top()).toBe(3072);
    expect(hold(), "the unchanged navigation left the restore reflow anchor armed").toBe("0");
    // Grow the card between the topmost reader row (n_33) and the old saved
    // anchor (n_35). First report samples the generic anchor onto n_33; a
    // second measurement pass then evaluates the hold.
    await act(async () => {
      geo.growById("obj_long_n_34", ROW + 40);
    });
    await act(async () => {
      geo.fireMeasure("obj_long_n_33");
      await geo.nextFrame();
    });
    expect(
      geo.top(),
      "the stale reflow anchor counter-scrolled a card the reader expanded below their anchor",
    ).toBe(3072);
  });

});
