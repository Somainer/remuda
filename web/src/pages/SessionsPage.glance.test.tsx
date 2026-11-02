/**
 * home.md §5.1/§5.2: /sessions lists top-level sessions only (SessionList
 * drops `parent != null`, and its caption says 「子 agent 不在此列」). ① must
 * count exactly that set, so the 工作中 tap can never show fewer rows than the
 * count promised.
 */
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { mockDb } from "../lib/mock";
import { hubStore } from "../lib/store";
import type { Instance } from "../types/instance";
import { known } from "../types/wire";
import { SessionsPage } from "./SessionsPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
}));

afterEach(() => {
  hubStore.logout();
  vi.restoreAllMocks();
});

it("counts 工作中 from the top-level sessions the list shows, not child agents", async () => {
  const host = {
    ...mockDb.hosts[0],
    id: "host-a",
    workspaces: [{ workspaceId: "wsp-a", hostId: "host-a", root: "/home/dev/ws" }],
  };
  const base = mockDb.instances[0];
  const parent = {
    ...base,
    id: "ins-parent",
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "running",
    connectivity: "connected",
    activity: known("working"),
    parent: null,
  } as Instance;
  const child = {
    ...base,
    id: "ins-child",
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "running",
    connectivity: "connected",
    activity: known("working"),
    parent: { instanceId: "ins-parent", runId: "run_1", commandId: "cmd_1" },
  } as Instance;

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [parent, child], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);

  await hubStore.bootstrap();
  await hubStore.refresh();
  expect(hubStore.getSnapshot().instances).toHaveLength(2);

  render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  // One card: the child session is not listed on /sessions.
  expect(screen.getAllByTestId("board-card")).toHaveLength(1);
  // So the count says 1, not 2 — the tapped list equals it.
  expect(await screen.findByTestId("glance-working")).toHaveTextContent("1 工作中");
});

it("mounts the ④ queue in the main column and hides it while the list search has text", async () => {
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  await hubStore.bootstrap();
  await hubStore.refresh();

  const { unmount } = render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  const queue = await screen.findByTestId("home-queue");
  expect(queue).toHaveAttribute("data-placement", "desktop");
  expect(queue).toHaveAttribute("id", "home-queue");
  unmount();

  render(
    <MemoryRouter initialEntries={["/sessions?q=gate"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  expect(screen.queryByTestId("home-queue")).toBeNull();
});
