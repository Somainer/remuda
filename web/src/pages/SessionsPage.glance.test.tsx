/**
 * home.md §5.1/§5.2/§5.3: the desktop ① row paints the ONE owner glance the
 * Hub serves (`GET /v1/owner/glance`), never a derivation over the instance
 * list. The unfiltered /sessions board leaves out the sessions an Agent
 * dispatched, so it can hold fewer rows than the owner-scope ● count — the
 * count is the owner's whole world, which is what ⑥ renders and what the
 * rank tap then lists.
 */
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { mockDb } from "../lib/mock";
import { hubStore } from "../lib/store";
import type { OwnerGlance } from "../types/generated";
import type { Instance } from "../types/instance";
import { known } from "../types/wire";
import { GLANCE_TARGETS } from "../features/home/glanceNav";
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
    doneUnseenIds: [],
    waiting: [],
    working: [],
    more: { waiting: 0, working: 0 },
  } as OwnerGlance;
}

it("paints the owner glance's 工作中 count, and the rank tap shows the same rows it counted", async () => {
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
  // One card: the child session is not a plain desktop row.
  expect(screen.getAllByTestId("board-card")).toHaveLength(1);
  // ① is the glance's number, not a re-derivation from that board.
  expect(await screen.findByTestId("glance-working")).toHaveTextContent("2 工作中");
  // The ● tap filters by the rank the COUNT used, which spans children too:
  // the tapped board shows 2 rows, matching the number (item 7).
  await userEvent.click(screen.getByTestId("glance-working"));
  await waitFor(() => expect(screen.getAllByTestId("board-card")).toHaveLength(2));
  expect(
    screen
      .getAllByTestId("board-card")
      .map((card) => card.getAttribute("data-rank")),
  ).toEqual(["working", "working"]);
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
  vi.spyOn(api, "ownerGlance").mockResolvedValue({
    ...glance({ doneUnseen: 1 }),
    doneUnseenIds: ["ins-done"],
  });
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

/**
 * [r3 item 3] The same tap, driven THROUGH mapInstance: a raw wire row carries
 * `parentInstanceId`, and the mapped `parent` is what the board's child rule
 * reads. Before the mapping every row read as a root, so the child never
 * appeared under a rank filter.
 */
it("maps parentInstanceId and filters the rank-tap board through mapInstance", async () => {
  const host = {
    ...mockDb.hosts[0],
    id: "host-wire",
    workspaces: [{ workspaceId: "wsp-wire", hostId: "host-wire", root: "/home/dev/ws" }],
  };
  const wire = (instanceId: string, parentInstanceId: string | null) => ({
    instanceId,
    hostId: "host-wire",
    workspaceId: "wsp-wire",
    kind: "claude",
    driver: "shell-pty",
    lifecycle: "running",
    activity: "working",
    connectivity: "connected",
    createdAt: "2026-10-30T09:00:00Z",
    updatedAt: "2026-10-30T10:00:00Z",
    title: instanceId,
    journalId: `obj_${instanceId}`,
    durableSeq: "1",
    exit: { state: "not-applicable" },
    parentInstanceId,
  });
  const records = [wire("ins_wire_parent", null), wire("ins_wire_child", "ins_wire_parent")];
  const realFetch = globalThis.fetch;
  vi.stubGlobal("fetch", async (input: string | URL | Request) => {
    const url = String(input);
    if (/\/v1\/instances(\?|$)/.test(url)) {
      return Response.json({ items: records, nextCursor: null });
    }
    return Response.json({});
  });

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(api, "ownerGlance").mockResolvedValue(glance({ working: 2 }));
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);

  try {
    await hubStore.bootstrap();
    await hubStore.refresh();
    await hubStore.refreshGlance();
    const mapped = hubStore.getSnapshot().instances;
    expect(mapped.find((row) => row.id === "ins_wire_parent")?.parent).toBeNull();
    expect(mapped.find((row) => row.id === "ins_wire_child")?.parent?.instanceId).toBe(
      "ins_wire_parent",
    );

    render(
      <MemoryRouter initialEntries={["/sessions?scope=all"]}>
        <SessionsPage />
      </MemoryRouter>,
    );
    await screen.findByTestId("session-list");
    // Unfiltered: the child is not a plain desktop row.
    expect(screen.getAllByTestId("board-card")).toHaveLength(1);
    // The ● tap counts the owner's world (child included) and the board now
    // shows exactly those rows.
    await userEvent.click(await screen.findByTestId("glance-working"));
    await waitFor(() => expect(screen.getAllByTestId("board-card")).toHaveLength(2));
  } finally {
    vi.stubGlobal("fetch", realFetch);
  }
});

/**
 * [r4 item 2] "Child" means DISPATCHED by an Agent, not "has a parent". A
 * D-026 resume writes `parentInstanceId` = the resumed session, so the
 * Hub's real shapes go through mapInstance here: the resumed conversation
 * stays on the board; a dispatched worker (and its own scoped respawn) is
 * hidden there and shown under the rank tap.
 */
it("keeps a resumed session on the board and folds only the dispatched worker", async () => {
  const host = {
    ...mockDb.hosts[0],
    id: "host-wire",
    workspaces: [{ workspaceId: "wsp-wire", hostId: "host-wire", root: "/home/dev/ws" }],
  };
  const wire = (instanceId: string, extra: Record<string, unknown>) => ({
    instanceId,
    hostId: "host-wire",
    workspaceId: "wsp-wire",
    kind: "claude",
    driver: "shell-pty",
    lifecycle: "running",
    activity: "working",
    connectivity: "connected",
    createdAt: "2026-10-30T09:00:00Z",
    updatedAt: "2026-10-30T10:00:00Z",
    title: instanceId,
    journalId: `obj_${instanceId}`,
    durableSeq: "1",
    exit: { state: "not-applicable" },
    parentInstanceId: null,
    ...extra,
  });
  const records = [
    wire("ins_root", { lifecycle: "exited", activity: "idle", creatorOrigin: "human" }),
    // The owner pressed Resume (http.rs resume_instance).
    wire("ins_resumed", {
      parentInstanceId: "ins_root",
      resumedFrom: "ins_root",
      creatorOrigin: "human",
    }),
    // The resumed conversation dispatched a worker…
    wire("ins_worker", { parentInstanceId: "ins_resumed", creatorOrigin: "agent" }),
    // …and that worker's scoped respawn carries resumedFrom too.
    wire("ins_worker_2", {
      parentInstanceId: "ins_resumed",
      resumedFrom: "ins_worker",
      creatorOrigin: "agent",
    }),
  ];
  const realFetch = globalThis.fetch;
  vi.stubGlobal("fetch", async (input: string | URL | Request) => {
    const url = String(input);
    if (/\/v1\/instances(\?|$)/.test(url)) {
      return Response.json({ items: records, nextCursor: null });
    }
    return Response.json({});
  });

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "passkeyList").mockResolvedValue({ items: [] } as never);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(api, "ownerGlance").mockResolvedValue(glance({ working: 3 }));
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);

  const cardIds = () =>
    screen
      .getAllByTestId("board-card")
      .map((card) => card.getAttribute("data-instance-id"))
      .sort();
  try {
    await hubStore.bootstrap();
    await hubStore.refresh();
    await hubStore.refreshGlance();
    const mapped = hubStore.getSnapshot().instances;
    const resumed = mapped.find((row) => row.id === "ins_resumed");
    expect(resumed?.parent?.instanceId).toBe("ins_root");
    expect(resumed?.resumedFrom).toBe("ins_root");
    expect(resumed?.creatorOrigin).toBe("human");
    expect(mapped.find((row) => row.id === "ins_worker")?.creatorOrigin).toBe("agent");

    render(
      <MemoryRouter initialEntries={["/sessions?scope=all"]}>
        <SessionsPage />
      </MemoryRouter>,
    );
    await screen.findByTestId("session-list");
    // Unfiltered: the resumed conversation is a plain row; the workers are not.
    expect(cardIds()).toEqual(["ins_resumed", "ins_root"]);
    // The ● tap shows every working row the count spans, workers included.
    await userEvent.click(await screen.findByTestId("glance-working"));
    await waitFor(() =>
      expect(cardIds()).toEqual(["ins_resumed", "ins_worker", "ins_worker_2"]),
    );
  } finally {
    vi.stubGlobal("fetch", realFetch);
  }
});

/**
 * [r4 item 5] 「子 agent 不在此列」 is a claim about the rows: it holds on the
 * unfiltered board and is false under a rank filter, which lists dispatched
 * workers too. The caption follows the filter.
 */
it("drops the 子 agent caption when a rank filter lists the workers", async () => {
  const host = {
    ...mockDb.hosts[0],
    id: "host-a",
    workspaces: [{ workspaceId: "wsp-a", hostId: "host-a", root: "/home/dev/ws" }],
  };
  const base = {
    ...mockDb.instances[0],
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "running",
    connectivity: "connected",
    activity: known("idle"),
    unseen: false,
  };
  const root = { ...base, id: "ins-root", parent: null, creatorOrigin: "human" } as Instance;
  const worker = {
    ...base,
    id: "ins-worker",
    parent: { instanceId: "ins-root", runId: null, commandId: null },
    creatorOrigin: "agent",
  } as Instance;

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [root, worker], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(api, "ownerGlance").mockResolvedValue(glance());
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  await hubStore.bootstrap();
  await hubStore.refresh();
  await hubStore.refreshGlance();

  const { unmount } = render(
    <MemoryRouter initialEntries={["/sessions?scope=all"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  expect(screen.getAllByTestId("board-card")).toHaveLength(1);
  expect(screen.getByTestId("session-group-meta-recent")).toHaveTextContent("子 agent 不在此列");
  unmount();

  render(
    <MemoryRouter initialEntries={["/sessions?scope=all&rank=idle"]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  expect(screen.getAllByTestId("board-card")).toHaveLength(2);
  expect(screen.getByTestId("session-group-meta-recent")).toHaveTextContent("2");
  expect(screen.getByTestId("session-group-meta-recent")).not.toHaveTextContent("子 agent");
});

/**
 * [r5 item 1] ⑥'s 「+N ›」 must open the set ⑥ counted. Among more than five
 * working rows sit a dispatched worker (the unfiltered board hides it) and a
 * session on a disconnected host (UiStatus calls it offline, not working); the
 * destination still lists exactly the glance's working set, and nothing else.
 */
it("opens ⑥'s +N on exactly the working set the glance counted", async () => {
  const hostA = {
    ...mockDb.hosts[0],
    id: "host-a",
    workspaces: [{ workspaceId: "wsp-a", hostId: "host-a", root: "/home/dev/ws" }],
  };
  const hostB = {
    ...mockDb.hosts[0],
    id: "host-b",
    state: "offline",
    workspaces: [{ workspaceId: "wsp-b", hostId: "host-b", root: "/home/dev/ws" }],
  };
  const base = {
    ...mockDb.instances[0],
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "running",
    connectivity: "connected",
    activity: known("working"),
    parent: null,
    creatorOrigin: "human",
  };
  const plain = ["ins-w1", "ins-w2", "ins-w3", "ins-w4", "ins-w5"].map((id) => ({ ...base, id }) as Instance);
  const child = {
    ...base,
    id: "ins-w-child",
    parent: { instanceId: "ins-w1", runId: null, commandId: null },
    creatorOrigin: "agent",
  } as Instance;
  const offline = {
    ...base,
    id: "ins-w-offline",
    hostId: "host-b",
    workspaceId: "wsp-b",
    connectivity: "disconnected",
  } as Instance;
  // Not counted: an idle session and an idle dispatched worker.
  const idle = { ...base, id: "ins-idle", activity: known("idle") } as Instance;
  const idleChild = { ...child, id: "ins-idle-child", activity: known("idle") } as Instance;
  const counted = [...plain, child, offline].map((row) => row.id as string);

  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [hostA, hostB], nextCursor: null } as never);
  vi.spyOn(api, "instanceList").mockResolvedValue({
    items: [...plain, child, offline, idle, idleChild],
    nextCursor: null,
  });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation(() => () => undefined);
  vi.spyOn(api, "ownerGlance").mockResolvedValue({
    ...glance({ working: counted.length }),
    working: counted.map((instanceId) => ({ instanceId, kind: "working", headline: instanceId })),
  });
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(hubStore, "flushAllOutbox").mockImplementation(() => undefined as never);
  await hubStore.bootstrap();
  await hubStore.refresh();
  await hubStore.refreshGlance();
  expect(counted).toHaveLength(7);

  render(
    <MemoryRouter initialEntries={[GLANCE_TARGETS.workingHref]}>
      <SessionsPage />
    </MemoryRouter>,
  );
  await screen.findByTestId("session-list");
  const ids = screen
    .getAllByTestId("board-card")
    .map((card) => card.getAttribute("data-instance-id"))
    .sort();
  expect(ids).toEqual([...counted].sort());
});
