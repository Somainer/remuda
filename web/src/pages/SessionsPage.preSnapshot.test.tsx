/**
 * home.md §8: `/sessions` must not paint a designed empty state before the
 * first successful list read. The offline-reload path (a stored device
 * session with the Hub unreachable) publishes `ready:true, authed:true` while
 * `hosts` and `instances` are still empty, so `ready` alone is not enough —
 * the no-host placeholder has to wait for the same freshness signal the list
 * uses.
 */
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { hubStore } from "../lib/store";
import { SessionsPage } from "./SessionsPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
}));

afterEach(() => {
  vi.restoreAllMocks();
});

it("an offline reload on /sessions shows the skeleton, never the no-hosts placeholder, until a read succeeds", async () => {
  // The Hub is unreachable on reload with a device session still present:
  // bootstrap takes the cached-session branch.
  vi.spyOn(api, "hello").mockRejectedValue(new Error("network off"));
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);

  await hubStore.bootstrap();
  const state = hubStore.getSnapshot();
  expect(state.ready).toBe(true);
  expect(state.authed).toBe(true);
  expect(state.hosts).toHaveLength(0);
  expect(state.instances).toHaveLength(0);
  // No snapshot from the Hub, so freshness is still unknown.
  expect(hubStore.getRefreshHealth().lastOkAt).toBeNull();

  render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  expect(screen.getByTestId("session-skeleton")).toBeInTheDocument();
  expect(screen.queryByText("无主机。请添加主机。")).toBeNull();

  // The next successful read (an empty Hub) makes the no-hosts state honest.
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  await hubStore.refresh();
  expect(await screen.findByText("无主机。请添加主机。")).toBeInTheDocument();
  expect(screen.queryByTestId("session-skeleton")).toBeNull();
});
