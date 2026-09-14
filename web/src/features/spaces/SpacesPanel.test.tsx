import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import { hubStore } from "../../lib/store";
import type { Instance } from "../../types/instance";
import { known } from "../../types/wire";
import { SpacesPanel } from "./SpacesPanel";
import { spaceKey, spaceStore, SPACES_PREFS_KEY, type Space } from "./store";

// jsdom has no matchMedia; QuickFind's viewport hook needs one. The panel
// tests are desktop tests, so it reports the desktop (non-mobile) layout.
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

vi.mock("../../lib/store", () => ({
  hubStore: { close: vi.fn(), deleteInstance: vi.fn().mockResolvedValue({ deleted: true, instanceId: "gone", nodePurge: "purged" }),
    resume: vi.fn(), toast: vi.fn(), titleOf: (id: string) => id, hostName: () => "host-a" },
  // QuickFind (mounted by the panel for its ⌘/Ctrl+K shortcut) reads the hub
  // snapshot; the panel tests exercise no finder behavior, so it gets an empty
  // live cache.
  useHub: () => ({ instances: [], hosts: [], workspaces: [], connection: "live" }),
}));

const alpha = spaceKey("host-a", "workspace-a");

function session(id: string, patch: Partial<Instance> = {}): Instance {
  return { ...mockDb.instances[0], id, hostId: "host-a", workspaceId: "workspace-a",
    lifecycle: "ready", connectivity: "connected", activity: known("idle"), ...patch };
}

const space: Space = {
  id: alpha, hostId: "host-a", workspaceId: "workspace-a", name: "alpha", liveCount: 1, blockedCount: 0,
  instances: [session("live"), session("gone", { lifecycle: "exited" })],
};

function renderPanel(instanceId?: string) {
  return render(<MemoryRouter>
    <SpacesPanel spaces={[space]} active={space} prefs={spaceStore.getSnapshot()} instanceId={instanceId} onSelect={() => {}} />
  </MemoryRouter>);
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(hubStore.deleteInstance).mockResolvedValue({ deleted: true, instanceId: "gone", nodePurge: "purged" });
  localStorage.removeItem(SPACES_PREFS_KEY);
  spaceStore.reload();
});

afterEach(() => { localStorage.removeItem(SPACES_PREFS_KEY); });

describe("SpacesPanel exited sessions", () => {
  it("collapses exited sessions by default and exposes resume and delete once opened", async () => {
    const user = userEvent.setup();
    const { rerender } = renderPanel();
    expect(screen.getByTestId("exited-toggle")).toHaveTextContent("已退出 (1)");
    expect(screen.getByTestId("exited-toggle")).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("exited-session")).not.toBeInTheDocument();
    // The live session stays listed inline rather than joining the group.
    expect(screen.getByTestId("space-session")).toHaveTextContent("live");

    await user.click(screen.getByTestId("exited-toggle"));
    rerender(<MemoryRouter>
      <SpacesPanel spaces={[space]} active={space} prefs={spaceStore.getSnapshot()} onSelect={() => {}} />
    </MemoryRouter>);
    expect(screen.getByTestId("exited-session")).toHaveAttribute("data-instance-id", "gone");
    await user.click(screen.getByTestId("exited-resume"));
    expect(hubStore.resume).toHaveBeenCalledWith("gone");
  });

  it("marks the current space and the current session as active", () => {
    renderPanel("live");
    const rows = screen.getAllByTestId("space-session");
    expect(rows[0]).toHaveAttribute("data-active", "true");
    expect(rows[0]).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("space-select")).toHaveAttribute("aria-pressed", "true");
  });

  it("confirms before deleting and only deletes after the confirmation", async () => {
    const user = userEvent.setup();
    spaceStore.toggleExited(alpha);
    renderPanel();
    await user.click(screen.getByTestId("exited-delete"));
    expect(screen.getByTestId("delete-session-sheet")).toHaveTextContent("删除会话及其记录？");
    await user.click(screen.getByTestId("delete-session-sheet-cancel"));
    expect(hubStore.deleteInstance).not.toHaveBeenCalled();

    await user.click(screen.getByTestId("exited-delete"));
    await user.click(screen.getByTestId("delete-session-confirm"));
    // An exited session needs no force, and nothing is closed separately.
    expect(hubStore.deleteInstance).toHaveBeenCalledWith("gone", false);
    expect(hubStore.close).not.toHaveBeenCalled();
    await waitFor(() => expect(hubStore.toast).toHaveBeenCalledWith("已删除会话"));
  });

  it("keeps the session listed and reports failure when the delete is refused", async () => {
    const user = userEvent.setup();
    vi.mocked(hubStore.deleteInstance).mockRejectedValueOnce(new Error("hub unavailable"));
    spaceStore.toggleExited(alpha);
    renderPanel();
    await user.click(screen.getByTestId("exited-delete"));
    await user.click(screen.getByTestId("delete-session-confirm"));

    await waitFor(() => expect(hubStore.toast).toHaveBeenCalledWith("删除失败，请重试"));
    // A failed delete must not look like a successful one.
    expect(hubStore.toast).not.toHaveBeenCalledWith("已删除会话");
    expect(screen.getByTestId("exited-session")).toHaveAttribute("data-instance-id", "gone");
  });

  it("does not claim the host data is gone when the Node could not purge it", async () => {
    const user = userEvent.setup();
    vi.mocked(hubStore.deleteInstance).mockResolvedValueOnce({ deleted: true, instanceId: "gone", nodePurge: "node-offline" });
    spaceStore.toggleExited(alpha);
    renderPanel();
    await user.click(screen.getByTestId("exited-delete"));
    await user.click(screen.getByTestId("delete-session-confirm"));

    // The Hub record is deleted either way; only the host copy is pending.
    await waitFor(() => expect(hubStore.toast).toHaveBeenCalledWith("已删除会话；该主机数据待其上线后清理"));
    expect(hubStore.toast).not.toHaveBeenCalledWith("已删除会话");
  });

  it("stops a running session before deleting it", async () => {
    const user = userEvent.setup();
    // A session that resumes while the sheet is open must be offered the
    // stop-first action, not the exited-only one.
    const running: Space = { ...space, instances: [session("live"), session("gone")] };
    spaceStore.toggleExited(alpha);
    const { rerender } = renderPanel();
    await user.click(screen.getByTestId("exited-delete"));
    rerender(<MemoryRouter>
      <SpacesPanel spaces={[running]} active={running} prefs={spaceStore.getSnapshot()} onSelect={() => {}} />
    </MemoryRouter>);

    expect(screen.queryByTestId("delete-session-confirm")).not.toBeInTheDocument();
    await user.click(screen.getByTestId("delete-session-stop"));
    // The Hub stops and deletes in one call, so the client must not close first.
    await waitFor(() => expect(hubStore.deleteInstance).toHaveBeenCalledWith("gone", true));
    expect(hubStore.close).not.toHaveBeenCalled();
  });
});
