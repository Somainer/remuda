import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Instance } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import { SPACES_PREFS_KEY, spaceStore } from "../spaces/store";
import { QuickFind, QuickFindTrigger, closeQuickFind } from "./QuickFind";

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
