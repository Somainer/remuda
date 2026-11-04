/**
 * home.md §5.1/§5.2/§5.3: the desktop ① row paints the ONE owner glance the
 * Hub serves (`GET /v1/owner/glance`), never a derivation over the instance
 * list. /sessions still lists top-level sessions only, so the board can hold
 * fewer rows than the owner-scope ● count — the count is the owner's whole
 * world, which is what ⑥ renders.
 */
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { mockDb } from "../lib/mock";
import { hubStore } from "../lib/store";
import type { OwnerGlance } from "../types/generated";
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

function glance(counts: Partial<OwnerGlance["counts"]> = {}): OwnerGlance {
  return {
    generatedAt: "2026-10-30T10:09:00.000Z",
    timezoneOffsetMinutes: 0,
    counts: { needYou: 0, doneUnseen: 0, working: 0, doneToday: 0, ...counts },
    waiting: [],
    working: [],
    more: { waiting: 0, working: 0 },
  } as unknown as OwnerGlance;
}

it("paints the owner glance's 工作中 count, while the board lists top-level sessions only", async () => {
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
  // The Hub's owner-scope count includes the child session the board hides.
  vi.spyOn(api, "ownerGlance").mockResolvedValue(glance({ working: 2 }));
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);

  await hubStore.bootstrap();
  await hubStore.refresh();
  await hubStore.refreshGlance();
  expect(hubStore.getSnapshot().instances).toHaveLength(2);

  render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  // One card: the child session is not a desktop row.
  expect(screen.getAllByTestId("board-card")).toHaveLength(1);
  // ① is the glance's number, not a re-derivation from that board.
  expect(await screen.findByTestId("glance-working")).toHaveTextContent("2 工作中");
});

it("filters the ledger to 完成未看 when ◉ is tapped, and clears it on the second tap", async () => {
  const host = {
    ...mockDb.hosts[0],
    id: "host-a",
    workspaces: [{ workspaceId: "wsp-a", hostId: "host-a", root: "/home/dev/ws" }],
  };
  const base = mockDb.instances[0];
  const done = {
    ...base, id: "ins-done", hostId: "host-a", workspaceId: "wsp-a",
    lifecycle: "running", connectivity: "connected", activity: known("idle"), parent: null, unseen: true,
  } as Instance;
  const working = {
    ...base, id: "ins-work", hostId: "host-a", workspaceId: "wsp-a",
    lifecycle: "running", connectivity: "connected", activity: known("working"), parent: null,
  } as Instance;

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [done, working], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(api, "ownerGlance").mockResolvedValue(glance({ doneUnseen: 1, working: 1 }));
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  await hubStore.bootstrap();
  await hubStore.refresh();
  await hubStore.refreshGlance();

  render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  expect(screen.getAllByTestId("board-card")).toHaveLength(2);
  const segment = await screen.findByTestId("glance-done-unseen");
  expect(segment).toHaveTextContent("1 完成未看");
  const user = userEvent.setup();
  await user.click(segment);
  expect(screen.getByTestId("glance-done-unseen")).toHaveAttribute("aria-pressed", "true");
  expect(screen.getAllByTestId("board-card")).toHaveLength(1);
  await user.click(screen.getByTestId("glance-done-unseen"));
  expect(screen.getByTestId("glance-done-unseen")).toHaveAttribute("aria-pressed", "false");
  expect(screen.getAllByTestId("board-card")).toHaveLength(2);
});

it("mounts ⑤ under the queue", async () => {
  const host = {
    ...mockDb.hosts[0], id: "host-a",
    workspaces: [{ workspaceId: "wsp-a", hostId: "host-a", root: "/home/dev/ws" }],
  };
  const done = {
    ...mockDb.instances[0], id: "ins-done", hostId: "host-a", workspaceId: "wsp-a",
    lifecycle: "running", connectivity: "connected", activity: known("idle"), parent: null, unseen: true,
  } as Instance;
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [done], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  await hubStore.bootstrap();
  await hubStore.refresh();

  render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  const section = await screen.findByTestId("home-just-done");
  const queue = screen.getByTestId("home-queue");
  expect(section).toHaveAttribute("data-placement", "desktop");
  expect(queue.compareDocumentPosition(section) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
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
