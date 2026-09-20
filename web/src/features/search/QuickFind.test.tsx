import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Instance } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import { SPACES_PREFS_KEY, spaceStore } from "../spaces/store";
import { QuickFind, QuickFindTrigger, closeQuickFind, openQuickFind } from "./QuickFind.tsx";

// vi.mock factories are hoisted above imports, so every value the mock closes
// over has to be hoisted with it. These are plain objects cast to the entity
// types: the finder render path only reads id/host/workspace/kind/timestamps
// plus the fields projectStatus reads.
const fixtures = vi.hoisted(() => {
  const instances = [
    {
      id: "ins_alpha000-0000-7000-8000-000000000001",
      hostId: "host-a",
      workspaceId: "wsp-a",
      kind: "claude",
      lifecycle: "ready",
      connectivity: "connected",
      activity: { state: "known", value: "idle" },
      createdAt: "2026-09-01T00:00:00Z",
      updatedAt: "2026-09-10T00:00:00Z",
    },
    {
      id: "ins_beta0000-0000-7000-8000-000000000002",
      hostId: "host-b",
      workspaceId: "wsp-b",
      kind: "claude",
      lifecycle: "ready",
      connectivity: "connected",
      activity: { state: "known", value: "idle" },
      createdAt: "2026-09-01T00:00:00Z",
      updatedAt: "2026-09-09T00:00:00Z",
    },
  ] as unknown as Instance[];
  const workspaces = [
    { id: "wsp-a", hostId: "host-a", label: "alpha", rootPath: "/tmp/alpha" },
    { id: "wsp-b", hostId: "host-b", label: "beta", rootPath: "/tmp/beta" },
  ] as unknown as Workspace[];
  const titles: Record<string, string> = {
    [instances[0].id]: "payments launch",
    [instances[1].id]: "incident retro",
  };
  const hub = { connection: "live" as "live" | "reconnecting" | "offline" };
  return { instances, workspaces, titles, hub };
});

vi.mock("../../lib/store", () => ({
  hubStore: {
    titleOf: (id: string) => fixtures.titles[id] ?? "会话",
    hostName: (id?: string) => (id === "host-b" ? "demo-node-2" : "demo-node-1"),
  },
  useHub: () => ({
    instances: fixtures.instances,
    workspaces: fixtures.workspaces,
    connection: fixtures.hub.connection,
  }),
}));

beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  });
});

function PathProbe() {
  const location = useLocation();
  return <div data-testid="path">{location.pathname}{location.search}</div>;
}

function renderFinder() {
  return render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <QuickFindTrigger />
      <QuickFind />
      <PathProbe />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  fixtures.hub.connection = "live";
  localStorage.removeItem(SPACES_PREFS_KEY);
  spaceStore.reload();
});

afterEach(() => {
  closeQuickFind();
  localStorage.removeItem(SPACES_PREFS_KEY);
});

function shortcut(target: Window | HTMLElement = window, init: KeyboardEventInit = {}) {
  // act(): the listener flips the finder's external store, which React must
  // flush before the assertions read the rendered panel.
  act(() => {
    target.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true, ...init }));
  });
}

describe("QuickFind", () => {
  it("opens from the trigger with the input focused and the cached rows listed", async () => {
    const user = userEvent.setup();
    renderFinder();
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();
    await user.click(screen.getByTestId("quickfind-trigger"));
    expect(screen.getByTestId("quickfind-panel")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByTestId("quickfind-input")).toHaveFocus());
    expect(screen.getAllByTestId("quickfind-result")).toHaveLength(2);
    // Every row carries Space + host, the same-title disambiguation fields.
    expect(screen.getAllByTestId("quickfind-result")[0]).toHaveTextContent(/alpha · demo-node-1/);
  });

  it("opens on Ctrl/Cmd+K but not while focus is in a typing surface or xterm", () => {
    renderFinder();
    shortcut();
    expect(screen.getByTestId("quickfind-panel")).toBeInTheDocument();
    act(() => closeQuickFind());
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();

    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    shortcut(input, { key: "k", metaKey: true, ctrlKey: false });
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();
    input.remove();

    const terminal = document.createElement("div");
    terminal.className = "xterm";
    terminal.tabIndex = 0;
    document.body.append(terminal);
    terminal.focus();
    shortcut(terminal);
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();
    terminal.remove();
  });

  it("filters and navigates with Enter without writing URL filters", async () => {
    const user = userEvent.setup();
    renderFinder();
    await user.click(screen.getByTestId("quickfind-trigger"));
    await user.type(screen.getByTestId("quickfind-input"), "retro");
    const rows = screen.getAllByTestId("quickfind-result");
    expect(rows).toHaveLength(1);
    expect(rows[0]).toHaveAttribute("data-instance-id", fixtures.instances[1].id);

    await user.keyboard("{Enter}");
    await waitFor(() => expect(screen.getByTestId("path")).toHaveTextContent(`/s/${fixtures.instances[1].id}`));
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();
    expect(screen.getByTestId("path")).not.toHaveTextContent("?");
  });

  it("moves the active row with ArrowDown/ArrowUp, wrapping at both ends", async () => {
    const user = userEvent.setup();
    renderFinder();
    shortcut();
    const box = screen.getByTestId("quickfind-input");
    await waitFor(() => expect(box).toHaveFocus());
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-0");
    await user.keyboard("{ArrowDown}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-1");
    expect(screen.getAllByTestId("quickfind-result")[1]).toHaveAttribute("data-selected", "true");
    await user.keyboard("{ArrowDown}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-0");
    await user.keyboard("{ArrowUp}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-1");
  });

  it("closes on Escape and returns focus to the trigger without touching the URL", async () => {
    const user = userEvent.setup();
    renderFinder();
    const trigger = screen.getByTestId("quickfind-trigger");
    await user.click(trigger);
    await user.type(screen.getByTestId("quickfind-input"), "payments");
    await user.keyboard("{Escape}");
    expect(screen.queryByTestId("quickfind-panel")).not.toBeInTheDocument();
    await waitFor(() => expect(trigger).toHaveFocus());
    expect(screen.getByTestId("path")).toHaveTextContent("/sessions");
  });

  it("offers a recovery path for zero matches", async () => {
    const user = userEvent.setup();
    renderFinder();
    await user.click(screen.getByTestId("quickfind-trigger"));
    await user.type(screen.getByTestId("quickfind-input"), "zzz-no-such-session");
    expect(screen.getByTestId("quickfind-empty")).toBeInTheDocument();
    await user.click(screen.getByTestId("quickfind-clear"));
    expect(screen.getAllByTestId("quickfind-result")).toHaveLength(2);
    await waitFor(() => expect(screen.getByTestId("quickfind-input")).toHaveFocus());
  });

  it("labels the cache-only scope when the Hub is down", async () => {
    const user = userEvent.setup();
    fixtures.hub.connection = "offline";
    renderFinder();
    await user.click(screen.getByTestId("quickfind-trigger"));
    expect(screen.getByTestId("quickfind-cache-only")).toHaveTextContent(/只搜索本机已缓存/);
  });
});

describe("QuickFind grouped Jump To (phone key bar)", () => {
  const orderKey = "remuda.mobile.quickfind.order.v1";

  function setCompact(compact: boolean) {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (query: string) => ({
        matches: compact,
        media: query,
        onchange: null,
        addEventListener: () => {},
        removeEventListener: () => {},
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      }),
    });
  }

  afterEach(() => {
    setCompact(false);
    fixtures.instances[0].activity = { state: "known", value: "idle" };
    fixtures.instances[0].updatedAt = "2026-09-10T00:00:00Z";
    fixtures.instances[1].updatedAt = "2026-09-09T00:00:00Z";
    for (const extra of fixtures.instances.splice(2)) {
      delete fixtures.titles[extra.id];
    }
    localStorage.removeItem(orderKey);
  });

  it("opens grouped on compact with project headers, branch and blocked count, sessions as leaves", async () => {
    setCompact(true);
    // The first cached session is blocked: buildSpaces counts it and the
    // header must advertise that exact count.
    fixtures.instances[0].activity = { state: "known", value: "waiting-interaction" };
    const user = userEvent.setup();
    renderFinder();
    act(() => openQuickFind({ grouped: true }));

    expect(screen.getByTestId("quickfind-panel")).toBeInTheDocument();
    const groups = screen.getAllByTestId("quickfind-group");
    expect(groups).toHaveLength(2);

    const first = groups[0]!;
    expect(first).toHaveTextContent("alpha");
    expect(first).toHaveTextContent("1 待处理");
    expect(first).toHaveAttribute("data-blocked", "1");
    expect(groups[1]).toHaveTextContent("beta");
    expect(groups[1]).toHaveTextContent("0 待处理");

    // The leaves are still plain session options inside the one listbox.
    expect(screen.getAllByTestId("quickfind-result")).toHaveLength(2);
    // The flat-list scope note yields its slot to the clock/list toggle.
    expect(screen.getByTestId("quickfind-order-clock")).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByText(/仅已加载的标题/)).not.toBeInTheDocument();

    // The combobox/listbox/activedescendant contract survives grouping.
    const box = screen.getByTestId("quickfind-input");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-0");
    await user.keyboard("{ArrowDown}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-1");
    expect(screen.getAllByTestId("quickfind-result")[1]).toHaveAttribute("data-selected", "true");
  });

  it("ArrowDown walks the visual grouped order even when it diverges from the flat recency array", async () => {
    setCompact(true);
    // Fixture where rankQuickFind's flat order is NOT the rendered order:
    //   alpha group (wsp-a): blocked oldest (09-10, pinned) + idle (09-12)
    //   beta group  (wsp-b): idle (09-15, newest single leaf)
    // Flat recency is [beta, alpha-idle, alpha-blocked]; grouped clock renders
    // the beta group first (latest leaf is newest), then alpha with the blocked
    // leaf pinned: [beta, alpha-blocked, alpha-idle].
    const alphaBlocked = fixtures.instances[0]!;
    const beta = fixtures.instances[1]!;
    alphaBlocked.updatedAt = "2026-09-10T00:00:00Z";
    alphaBlocked.activity = { state: "known", value: "waiting-interaction" };
    beta.updatedAt = "2026-09-15T00:00:00Z";
    const alphaIdle = {
      id: "ins_alpha002-0000-7000-8000-000000000003",
      hostId: "host-a",
      workspaceId: "wsp-a",
      kind: "claude",
      lifecycle: "ready",
      connectivity: "connected",
      activity: { state: "known", value: "idle" },
      createdAt: "2026-09-01T00:00:00Z",
      updatedAt: "2026-09-12T00:00:00Z",
    } as unknown as Instance;
    fixtures.instances.push(alphaIdle);
    fixtures.titles[alphaIdle.id] = "middle alpha row";

    const user = userEvent.setup();
    renderFinder();
    act(() => openQuickFind({ grouped: true }));

    const rowsInVisualOrder = () =>
      Array.from(
        document.querySelectorAll<HTMLElement>("#quickfind-listbox [role='option']"),
      );
    const instanceIdsInVisualOrder = () =>
      rowsInVisualOrder().map((row) => row.getAttribute("data-instance-id"));

    // Rendered (document) order is the grouped order, and the option ids match
    // that reading order — option-1 is the pinned blocked row, not flat[1].
    expect(instanceIdsInVisualOrder()).toEqual([beta.id, alphaBlocked.id, alphaIdle.id]);
    expect(rowsInVisualOrder().map((row) => row.id)).toEqual([
      "quickfind-option-0",
      "quickfind-option-1",
      "quickfind-option-2",
    ]);

    const box = screen.getByTestId("quickfind-input");
    const selectedId = () =>
      rowsInVisualOrder()
        .find((row) => row.getAttribute("data-selected") === "true")
        ?.getAttribute("data-instance-id");

    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-0");
    expect(selectedId()).toBe(beta.id);

    // One ArrowDown must highlight the visually next row — the blocked row
    // pinned at the top of the second group — not flat[1] (the 09-12 row).
    await user.keyboard("{ArrowDown}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-1");
    expect(selectedId()).toBe(alphaBlocked.id);
    expect(
      rowsInVisualOrder().find((row) => row.getAttribute("data-instance-id") === alphaIdle.id),
    ).toHaveAttribute("data-selected", "false");

    // Second ArrowDown lands on the visually third row; ArrowUp reverses.
    await user.keyboard("{ArrowDown}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-2");
    expect(selectedId()).toBe(alphaIdle.id);
    await user.keyboard("{ArrowUp}");
    expect(box).toHaveAttribute("aria-activedescendant", "quickfind-option-1");
    expect(selectedId()).toBe(alphaBlocked.id);
  });

  it("remembers the clock/list choice per device", async () => {
    setCompact(true);
    const user = userEvent.setup();
    let rendered = renderFinder();
    act(() => openQuickFind({ grouped: true }));
    await user.click(screen.getByTestId("quickfind-order-list"));
    expect(screen.getByTestId("quickfind-order-list")).toHaveAttribute("aria-pressed", "true");
    expect(localStorage.getItem(orderKey)).toBe("list");
    act(() => closeQuickFind());
    rendered.unmount();

    // A fresh mount reads the persisted choice.
    rendered = renderFinder();
    act(() => openQuickFind({ grouped: true }));
    expect(screen.getByTestId("quickfind-order-list")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("quickfind-order-clock")).toHaveAttribute("aria-pressed", "false");
  });

  it("ignores the grouped entry on desktop and for the plain trigger, keeping the flat list", () => {
    // Desktop viewport even when the caller asked for grouped.
    setCompact(false);
    renderFinder();
    act(() => openQuickFind({ grouped: true }));
    expect(screen.queryAllByTestId("quickfind-group")).toHaveLength(0);
    expect(screen.getAllByTestId("quickfind-result")).toHaveLength(2);
    expect(screen.queryByTestId("quickfind-order-clock")).not.toBeInTheDocument();
    expect(screen.getByText(/仅已加载的标题/)).toBeInTheDocument();
    act(() => closeQuickFind());

    // Compact but the plain opener (spaces drawer trigger / ⌘K) stays flat.
    setCompact(true);
    act(() => openQuickFind());
    expect(screen.queryAllByTestId("quickfind-group")).toHaveLength(0);
    expect(screen.getAllByTestId("quickfind-result")).toHaveLength(2);
  });
});
