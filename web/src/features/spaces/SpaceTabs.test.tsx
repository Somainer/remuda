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
      lifecycle: "ready", connectivity: "connected", activity: known("idle") }] },
];

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
    await user.click(screen.getByRole("button", { name: "关闭会话 a1" }));
    expect(hubStore.close).toHaveBeenCalledWith("a1");
    expect(screen.getByRole("button", { name: "关闭会话 a1" })).toBeDisabled();
    const otherProject = screen.getByRole("button", { name: "切换 beta" });
    await user.click(otherProject);
    expect(screen.getByRole("tab", { name: /b1/ })).toHaveAttribute("aria-selected", "true");

    await act(async () => { pending.resolve(); });

    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/b1");
    expect(spaceStore.getSnapshot()).toMatchObject({ selectedSpaceId: beta, selectedTabs: { [beta]: "b1" }, closedTabs: { [alpha]: ["a1"] } });
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
    await user.click(screen.getByRole("button", { name: "关闭会话 a1" }));

    await act(async () => { pending.reject(new Error("node unavailable")); });

    expect(hubStore.toast).toHaveBeenCalledWith("关闭失败，请重试");
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/a1");
    expect(screen.getByRole("tab", { name: /a1/ })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("button", { name: "关闭会话 a1" })).toBeEnabled();
    expect(spaceStore.getSnapshot().closedTabs[alpha] ?? []).toEqual([]);
  });

  it("focuses the successor tab after a keyboard-initiated close", async () => {
    const user = userEvent.setup();
    const pending = deferredClose();
    renderWorkbench();
    screen.getByRole("button", { name: "关闭会话 a1" }).focus();
    await user.keyboard("{Enter}");

    await act(async () => { pending.resolve(); });

    const successor = screen.getByRole("tab", { name: /a2/ });
    expect(screen.getByTestId("current-route")).toHaveTextContent("/s/a2");
    expect(successor).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect(successor).toHaveFocus());
    expect(screen.queryByRole("tab", { name: /a1/ })).not.toBeInTheDocument();
  });
});
