import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DELETE_UNSUPPORTED } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import { mockDb } from "../../lib/mock";
import { hubStore } from "../../lib/store";
import type { Instance } from "../../types/instance";
import { known } from "../../types/wire";
import { SpacesPanel } from "./SpacesPanel";
import { spaceKey, spaceStore, SPACES_PREFS_KEY, type Space } from "./store";

vi.mock("../../lib/store", () => ({
  hubStore: { close: vi.fn(), deleteInstance: vi.fn(), resume: vi.fn(), toast: vi.fn(),
    titleOf: (id: string) => id, hostName: () => "host-a" },
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
    expect(hubStore.deleteInstance).toHaveBeenCalledWith("gone");
    // An exited session has nothing left to stop.
    expect(hubStore.close).not.toHaveBeenCalled();
    await waitFor(() => expect(hubStore.toast).toHaveBeenCalledWith("已删除会话"));
    expect(spaceStore.getSnapshot().hiddenSessions[alpha] ?? []).toEqual([]);
  });

  it("hides the row and says so when the Hub has no delete route yet", async () => {
    const user = userEvent.setup();
    vi.mocked(hubStore.deleteInstance).mockRejectedValueOnce(new HubHttpError(405, DELETE_UNSUPPORTED, "no route"));
    spaceStore.toggleExited(alpha);
    renderPanel();
    await user.click(screen.getByTestId("exited-delete"));
    await user.click(screen.getByTestId("delete-session-confirm"));

    await waitFor(() => expect(spaceStore.getSnapshot().hiddenSessions[alpha]).toEqual(["gone"]));
    // The record survives on the Hub, so the message must not claim a deletion.
    expect(hubStore.toast).toHaveBeenCalledWith("当前 Hub 尚不支持删除，已从本设备列表隐藏");
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
    expect(hubStore.close).toHaveBeenCalledWith("gone");
    await waitFor(() => expect(hubStore.deleteInstance).toHaveBeenCalledWith("gone"));
  });
});
