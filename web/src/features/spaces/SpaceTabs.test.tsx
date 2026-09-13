import { useEffect, useLayoutEffect } from "react";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation, useNavigate } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import { hubStore } from "../../lib/store";
import { known } from "../../types/wire";
import { SpaceTabs } from "./SpaceTabs";
import {
  newSessionPath, selectedSpace, selectedTab, spaceKey, spaceStore, SPACES_PREFS_KEY,
  useSpacesPrefs, visibleTabs, type Space,
} from "./store";

vi.mock("../../lib/store", () => ({
  hubStore: { close: vi.fn(), toast: vi.fn(), titleOf: (id: string) => id },
}));

const alpha = spaceKey("host-a", "workspace-a");
const beta = spaceKey("host-a", "workspace-b");
const spaces: Space[] = [
  { id: alpha, hostId: "host-a", workspaceId: "workspace-a", name: "alpha", liveCount: 2, blockedCount: 0,
    instances: ["a1", "a2"].map((id) => ({ ...mockDb.instances[0], id, hostId: "host-a", workspaceId: "workspace-a",
      lifecycle: "ready", connectivity: "connected", activity: known("idle") })) },
  { id: beta, hostId: "host-a", workspaceId: "workspace-b", name: "beta", liveCount: 1, blockedCount: 0,
    instances: [{ ...mockDb.instances[0], id: "b1", hostId: "host-a", workspaceId: "workspace-b",
      lifecycle: "ready", connectivity: "connected", activity: known("idle") },
    { ...mockDb.instances[0], id: "b-exited", hostId: "host-a", workspaceId: "workspace-b",
      lifecycle: "exited", connectivity: "connected", activity: known("idle") }] },
];

/** The strip asks before stopping anything; take the "stop and close" branch. */
async function stopAndClose(user: ReturnType<typeof userEvent.setup>, title: string) {
  await user.click(screen.getByRole("button", { name: `关闭标签 ${title}` }));
  await user.click(screen.getByTestId("tab-close-stop"));
}

function deferredClose() {
  let resolve!: () => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<void>((accept, fail) => { resolve = accept; reject = fail; });
  vi.mocked(hubStore.close).mockReturnValueOnce(promise);
  return { resolve, reject };
}

function Workbench() {
  const location = useLocation();
  const navigate = useNavigate();
  const prefs = useSpacesPrefs();
  const instanceId = location.pathname.split("/")[2];
  const active = selectedSpace(spaces, prefs, instanceId)!;
  // Production uses BrowserRouter; mirror its address-bar update for the
  // component's async navigation guard while keeping this test in MemoryRouter.
  useLayoutEffect(() => { window.history.replaceState(null, "", location.pathname); }, [location.pathname]);
  useEffect(() => {
    if (instanceId) spaceStore.selectTab(active.id, instanceId);
  }, [active.id, instanceId]);
  return <>
    <output data-testid="current-route">{location.pathname}</output>
    {spaces.map((space) => <button key={space.id} type="button" onClick={() => {
      spaceStore.selectSpace(space.id);
      const tab = selectedTab(space, spaceStore.getSnapshot());
      navigate(tab ? `/s/${tab.id}` : "/sessions");
    }}>切换 {space.name}</button>)}
    <SpaceTabs space={active} tabs={visibleTabs(active, prefs)} prefs={prefs} instanceId={instanceId} newHref={newSessionPath(active)} />
  </>;
}

const scrollIntoView = Object.getOwnPropertyDescriptor(Element.prototype, "scrollIntoView");

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.removeItem(SPACES_PREFS_KEY);
  spaceStore.reload();
  Object.defineProperty(Element.prototype, "scrollIntoView", { configurable: true, value: vi.fn() });
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => window.setTimeout(() => callback(0), 0));
});

afterEach(() => {
  localStorage.removeItem(SPACES_PREFS_KEY);
  window.history.replaceState(null, "", "/");
  if (scrollIntoView) Object.defineProperty(Element.prototype, "scrollIntoView", scrollIntoView);
  else Reflect.deleteProperty(Element.prototype, "scrollIntoView");
  vi.unstubAllGlobals();
});

function renderWorkbench() {
  return render(<MemoryRouter initialEntries={["/s/a1"]}><Workbench /></MemoryRouter>);
}

describe("SpaceTabs asynchronous closure", () => {
  it("keeps the other project's route, selection and focus when a delayed close completes", async () => {
    const user = userEvent.setup();
    const pending = deferredClose();
    renderWorkbench();
    await stopAndClose(user, "a1");
    expect(hubStore.close).toHaveBeenCalledWith("a1");
    expect(screen.getByRole("button", { name: "关闭标签 a1" })).toBeDisabled();
    const otherProject = screen.getByRole("button", { name: "切换 beta" });
    await user.click(otherProject);
    expect(screen.getByRole("tab", { name: /b1/ })).toHaveAttribute("aria-selected", "true");

    await act(async () => { pending.resolve(); });

    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/b1");
    expect(spaceStore.getSnapshot()).toMatchObject({ selectedSpaceId: beta, selectedTabs: { [beta]: "b1" },
      closedTabs: { [alpha]: [{ id: "a1", resurface: false }] } });
    expect(otherProject).toHaveFocus();
    expect(hubStore.toast).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "切换 alpha" }));
    expect(screen.getByRole("tab", { name: /a2/ })).toHaveAttribute("aria-selected", "true");
    expect(screen.queryByRole("tab", { name: /a1/ })).not.toBeInTheDocument();
  });

  it("retains the selected tab and reports failure when close rejects", async () => {
    const user = userEvent.setup();
    const pending = deferredClose();
    renderWorkbench();
    await stopAndClose(user, "a1");

    await act(async () => { pending.reject(new Error("node unavailable")); });

    expect(hubStore.toast).toHaveBeenCalledWith("停止失败，请重试");
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/a1");
    expect(screen.getByRole("tab", { name: /a1/ })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("button", { name: "关闭标签 a1" })).toBeEnabled();
    expect(spaceStore.getSnapshot().closedTabs[alpha] ?? []).toEqual([]);
  });

  it("removes an exited tab without asking and without stopping anything", async () => {
    const user = userEvent.setup();
    render(<MemoryRouter initialEntries={["/s/b-exited"]}><Workbench /></MemoryRouter>);
    await user.click(screen.getByRole("button", { name: "关闭标签 b-exited" }));

    // An exited session has nothing to stop, so no sheet and no close command.
    expect(screen.queryByTestId("tab-close-sheet")).not.toBeInTheDocument();
    expect(hubStore.close).not.toHaveBeenCalled();
    expect(screen.queryByRole("tab", { name: /b-exited/ })).not.toBeInTheDocument();
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/b1");
  });

  it("keeps a running session alive when the user only dismisses its tab, and cancels leave everything", async () => {
    const user = userEvent.setup();
    renderWorkbench();
    await user.click(screen.getByRole("button", { name: "关闭标签 a1" }));
    await user.click(screen.getByTestId("tab-close-sheet-cancel"));
    expect(screen.getByRole("tab", { name: /a1/ })).toHaveAttribute("aria-selected", "true");
    expect(spaceStore.getSnapshot().closedTabs[alpha] ?? []).toEqual([]);

    await user.click(screen.getByRole("button", { name: "关闭标签 a1" }));
    await user.click(screen.getByTestId("tab-close-keep"));

    expect(hubStore.close).not.toHaveBeenCalled();
    // The tab is gone but the dismissal is armed to bring it back when blocked.
    expect(spaceStore.getSnapshot().closedTabs[alpha]).toEqual([{ id: "a1", resurface: true }]);
    expect(screen.queryByRole("tab", { name: /a1/ })).not.toBeInTheDocument();
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/a2");
  });

  it("brings a dismissed tab back once its session needs a human", async () => {
    const user = userEvent.setup();
    const { rerender } = renderWorkbench();
    await user.click(screen.getByRole("button", { name: "关闭标签 a1" }));
    await user.click(screen.getByTestId("tab-close-keep"));
    expect(screen.queryByRole("tab", { name: /a1/ })).not.toBeInTheDocument();

    act(() => { spaces[0].instances[0].activity = known("waiting-interaction"); });
    rerender(<MemoryRouter initialEntries={["/s/a2"]}><Workbench /></MemoryRouter>);

    expect(screen.getByRole("tab", { name: /a1/ })).toBeInTheDocument();
    spaces[0].instances[0].activity = known("idle");
  });

  it("focuses the successor tab after a keyboard-initiated close", async () => {
    const user = userEvent.setup();
    const pending = deferredClose();
    renderWorkbench();
    screen.getByRole("button", { name: "关闭标签 a1" }).focus();
    await user.keyboard("{Enter}");
    await user.keyboard("{Enter}");

    await act(async () => { pending.resolve(); });

    const successor = screen.getByRole("tab", { name: /a2/ });
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/a2");
    expect(successor).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect(successor).toHaveFocus());
    expect(screen.queryByRole("tab", { name: /a1/ })).not.toBeInTheDocument();
  });
});
