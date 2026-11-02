import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HubState } from "../../lib/store";
import { HubHttpError } from "../../lib/httpError";
import {
  NOW,
  PLAN,
  T0,
  T1,
  T2,
  host,
  inst,
  interaction,
  question,
  task,
} from "./queueTestData";

/**
 * The ④ queue's render contract (home.md §4.1, §4.4, §8, §9 落点): the
 * collapsed/expanded queue, the no-optimism answer flow with its 2 s receipt
 * and the 409/410 「已在其他设备处理」 exit, stale greying, and the ?focus
 * highlight. The card kinds themselves are ApprovalCard.home.test.tsx.
 */

const fake = vi.hoisted(() => ({
  state: null as unknown,
  tasks: [] as unknown[],
  freshness: { state: "fresh", lastOkAt: 1 as number | null, failingSince: null as number | null },
  respond: vi.fn(),
  refresh: vi.fn(),
  markSeen: vi.fn(),
}));

vi.mock("../../lib/store", () => ({
  useHub: () => fake.state,
  hubStore: {
    titleOf: (id: string) => id,
    hostName: () => "forge",
    getSnapshot: () => fake.state,
    respond: fake.respond,
    refresh: fake.refresh,
    markSeen: fake.markSeen,
  },
}));

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
  HOME_FRESHNESS: { pollMs: 2_000, staleAfterMs: 10_000, bannerAfterMs: 5_000, receiptMs: 2_000 },
}));

vi.mock("../tasks/taskLedger", () => ({
  useTaskLedger: () => ({ tasks: fake.tasks, projectName: () => "remuda" }),
}));

vi.mock("../mobile/useInboxPendingCount", () => ({
  useInboxClockNow: () => NOW,
}));

import { HomeQueue } from "./HomeQueue";
import { answeredElsewhere } from "./homeQueue";

function hub(overrides: Partial<HubState> = {}): HubState {
  return {
    instances: [],
    interactions: [],
    hosts: [host({ id: "hst_1" })],
    workspaces: [{ id: "wsp_1", label: "remuda" }],
    answering: {},
    screens: {},
    ...overrides,
  } as unknown as HubState;
}

function renderQueue(placement: "phone" | "desktop" = "phone", url = "/m", searching = false) {
  return render(
    <MemoryRouter initialEntries={[url]}>
      <HomeQueue placement={placement} searching={searching} />
    </MemoryRouter>,
  );
}

function rerenderQueue(view: ReturnType<typeof render>) {
  view.rerender(
    <MemoryRouter initialEntries={["/m"]}>
      <HomeQueue placement="phone" searching={false} />
    </MemoryRouter>,
  );
}

function cards() {
  return screen.queryAllByTestId("home-queue-card");
}

beforeEach(() => {
  fake.tasks = [];
  fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
  fake.respond.mockReset();
  fake.refresh.mockReset().mockResolvedValue(undefined);
  fake.markSeen.mockReset().mockResolvedValue(null);
  Element.prototype.scrollIntoView = vi.fn();
});

afterEach(() => {
  vi.useRealTimers();
});

/* ------------------------------------------------------------------ */
/* The queue: order, fold, links, search, answer flow, focus.         */
/* ------------------------------------------------------------------ */

function fiveCards(): HubState {
  return hub({
    instances: [inst({ id: "ins_a" }), inst({ id: "ins_b" }), inst({ id: "ins_c" }), inst({ id: "ins_d" })],
    interactions: [
      interaction({ id: "int_1", instanceId: "ins_a", createdAt: T2 }),
      interaction({ id: "int_2", instanceId: "ins_b", createdAt: T1, request: question() }),
      interaction({ id: "int_3", instanceId: "ins_c", createdAt: T0, request: PLAN }),
      interaction({ id: "int_4", instanceId: "ins_d", createdAt: "2026-10-31T07:00:00.000Z" }),
    ],
  });
}

describe("HomeQueue (§4.1)", () => {
  it("shows two skeleton cards before the first snapshot, never an empty state", () => {
    fake.state = hub();
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    renderQueue();
    expect(screen.getAllByTestId("home-queue-skeleton")).toHaveLength(2);
    expect(screen.queryByText("需要你 · 0")).toBeNull();
  });

  it("collapses to one 需要你 · 0 line when nothing is waiting", () => {
    fake.state = hub();
    renderQueue();
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 0");
    expect(cards()).toHaveLength(0);
    expect(screen.queryByTestId("home-queue-inbox")).toBeNull();
  });

  it("shows 3 cards on the phone and expands the rest in place, in §4.1 order", () => {
    fake.state = fiveCards();
    fake.tasks = [task({ id: "tsk_b", blockedReason: "等你确认验收范围" })];
    renderQueue("phone");
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 5");
    expect(cards().map((card) => card.getAttribute("data-card-id"))).toEqual(["int_1", "int_2", "int_3"]);
    const more = screen.getByTestId("home-queue-more");
    expect(more).toHaveTextContent("还有 2 条 ▾");
    fireEvent.click(more);
    expect(cards().map((card) => card.getAttribute("data-card-id"))).toEqual([
      "int_1",
      "int_2",
      "int_3",
      "int_4",
      "tsk_b",
    ]);
    expect(screen.getByTestId("home-queue-more")).toHaveTextContent("收起 ▴");
    const taskCard = cards()[4];
    expect(taskCard).toHaveAttribute("data-card-type", "task");
    expect(within(taskCard).getByText(/任务待你 · SE-01/)).toBeInTheDocument();
    expect(within(taskCard).getByTestId("home-card-task")).toHaveTextContent("打开任务");
  });

  it("shows 4 cards on desktop and links 去收件箱 to the earliest interaction", () => {
    fake.state = fiveCards();
    fake.tasks = [task({ id: "tsk_b", blockedReason: "等你确认验收范围" })];
    renderQueue("desktop", "/sessions");
    expect(cards()).toHaveLength(4);
    expect(screen.getByTestId("home-queue-more")).toHaveTextContent("还有 1 条 ▾");
    expect(screen.getByTestId("home-queue-inbox")).toHaveAttribute("href", "/approvals?focus=int_4");
  });

  it("is hidden while the ledger search has text", () => {
    fake.state = fiveCards();
    renderQueue("phone", "/m", true);
    expect(screen.queryByTestId("home-queue")).toBeNull();
  });

  it("greys 知道了 with 数据可能过期 while the queue is stale (§8)", () => {
    fake.state = hub({
      instances: [inst({ id: "ins_f", lastTurnError: { at: T1, text: "API Error: 529" }, unseen: true })],
    });
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 1 };
    renderQueue();
    const card = cards()[0];
    expect(within(card).getByTestId("home-card-ack")).toBeDisabled();
    expect(within(card).getByTestId("home-card-stale")).toHaveTextContent("数据可能过期");
    // 打开 only navigates, so it stays live (§4.4).
    expect(within(card).getByTestId("home-card-open")).not.toHaveAttribute("aria-disabled");
    fireEvent.click(within(card).getByTestId("home-card-ack"));
    expect(fake.markSeen).not.toHaveBeenCalled();
  });

  it("renders a failed turn with 打开 and 知道了 (seen), outside the count", async () => {
    fake.state = hub({
      instances: [inst({ id: "ins_f", lastTurnError: { at: T1, text: "API Error: 529" }, unseen: true })],
    });
    renderQueue();
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 0");
    const card = cards()[0];
    expect(card).toHaveAttribute("data-card-type", "failed");
    expect(within(card).getByTestId("home-card-open")).toHaveAttribute("href", "/s/ins_f");
    await act(async () => {
      fireEvent.click(within(card).getByTestId("home-card-ack"));
    });
    expect(fake.markSeen).toHaveBeenCalledWith("ins_f");
  });
});

describe("HomeQueue holds the order during interaction (§3.3, §4.1)", () => {
  function fiveInteractions(): HubState {
    return hub({
      instances: ["ins_a", "ins_b", "ins_c", "ins_d", "ins_e"].map((id) => inst({ id })),
      interactions: [
        interaction({ id: "int_1", instanceId: "ins_a", createdAt: T2 }),
        interaction({ id: "int_2", instanceId: "ins_b", createdAt: T1 }),
        interaction({ id: "int_3", instanceId: "ins_c", createdAt: T0 }),
        interaction({ id: "int_4", instanceId: "ins_d", createdAt: "2026-10-31T07:00:00.000Z" }),
        interaction({ id: "int_5", instanceId: "ins_e", createdAt: "2026-10-31T06:00:00.000Z" }),
      ],
    });
  }

  function paintedIds() {
    return cards().map((card) => card.getAttribute("data-card-id"));
  }

  it("freezes the expanded queue when a newer card arrives, and re-orders on collapse", () => {
    fake.state = fiveInteractions();
    const view = renderQueue("phone");
    fireEvent.click(screen.getByTestId("home-queue-more"));
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "1");
    expect(paintedIds()).toEqual(["int_1", "int_2", "int_3", "int_4", "int_5"]);

    // A 2 s poll lands a newer interaction (and drops one) while the queue
    // is open: the painted order must not move under the reader.
    const next = fiveInteractions();
    fake.state = hub({
      instances: next.instances,
      interactions: [
        interaction({ id: "int_0", instanceId: "ins_a", createdAt: "2026-10-31T11:00:00.000Z" }),
        interaction({ id: "int_1", instanceId: "ins_b", createdAt: T2 }),
        interaction({ id: "int_2", instanceId: "ins_c", createdAt: T1 }),
        interaction({ id: "int_3", instanceId: "ins_d", createdAt: T0 }),
        interaction({ id: "int_4", instanceId: "ins_e", createdAt: "2026-10-31T07:00:00.000Z" }),
      ],
    });
    rerenderQueue(view);
    expect(paintedIds()).toEqual(["int_1", "int_2", "int_3", "int_4", "int_5"]);
    // The card that vanished from the live data is an inert placeholder.
    expect(cards()[4]).toHaveAttribute("data-stale", "1");
    // The new card waits for the release.
    expect(paintedIds()).not.toContain("int_0");

    // Collapsing releases the hold: the queue re-orders once, back to the
    // collapsed 3-card slice with the new card first.
    fireEvent.click(screen.getByTestId("home-queue-more"));
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
    expect(paintedIds()).toEqual(["int_0", "int_1", "int_2"]);
  });
});

describe("HomeQueue answer flow (§4.4)", () => {
  function oneApproval() {
    fake.state = hub({
      instances: [inst({ id: "ins_a" })],
      interactions: [interaction({ id: "int_a", instanceId: "ins_a" })],
    });
  }

  it("shows 已提交 for 2 s after a 200, then the card leaves", async () => {
    vi.useFakeTimers();
    oneApproval();
    fake.respond.mockResolvedValue(undefined);
    renderQueue();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "允许一次" }));
    });
    expect(fake.respond).toHaveBeenCalledWith("int_a", expect.objectContaining({ optionId: "allow-once" }));
    expect(screen.getByTestId("home-card-receipt")).toHaveTextContent("已提交");
    expect(screen.queryByRole("button", { name: "允许一次" })).toBeNull();
    await act(async () => {
      vi.advanceTimersByTime(1_999);
    });
    expect(screen.getByTestId("home-card-receipt")).toBeInTheDocument();
    await act(async () => {
      vi.advanceTimersByTime(1);
    });
    // The fake store still lists it pending (no poll yet): the card left anyway.
    expect(cards()).toHaveLength(0);
  });

  it("keeps the card's slot while the store drops the committed row before respond() resolves", async () => {
    vi.useFakeTimers();
    oneApproval();
    let resolve: () => void = () => undefined;
    fake.respond.mockImplementation(() => new Promise<void>((done) => (resolve = done)));
    const view = renderQueue();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "允许一次" }));
    });
    // The real store flips the row to committed (and out of the queue) before
    // its follow-up refresh lets respond() resolve.
    fake.state = hub({ instances: [inst({ id: "ins_a" })], interactions: [] });
    view.rerender(
      <MemoryRouter initialEntries={["/m"]}>
        <HomeQueue placement="phone" searching={false} />
      </MemoryRouter>,
    );
    expect(cards()).toHaveLength(1);
    expect(screen.getByRole("button", { name: "允许一次" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "拒绝" })).toBeDisabled();
    await act(async () => {
      resolve();
    });
    expect(screen.getByTestId("home-card-receipt")).toHaveTextContent("已提交");
    await act(async () => {
      vi.advanceTimersByTime(2_000);
    });
    expect(cards()).toHaveLength(0);
  });

  it.each([409, 410])("shows 已在其他设备处理 on %i, then the card leaves", async (status) => {
    vi.useFakeTimers();
    oneApproval();
    fake.respond.mockRejectedValue(new HubHttpError(status, "INTERACTION_SUPERSEDED", "superseded"));
    renderQueue();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "拒绝" }));
    });
    expect(screen.getByTestId("home-card-receipt")).toHaveTextContent("已在其他设备处理");
    expect(screen.queryByTestId("home-card-error")).toBeNull();
    expect(fake.refresh).toHaveBeenCalled();
    await act(async () => {
      vi.advanceTimersByTime(2_000);
    });
    expect(cards()).toHaveLength(0);
  });

  it("shows any other error inline and re-enables the buttons", async () => {
    oneApproval();
    fake.respond.mockRejectedValue(new HubHttpError(500, "INTERNAL", "boom"));
    renderQueue();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "允许一次" }));
    });
    expect(screen.getByTestId("home-card-error")).toHaveTextContent("boom");
    expect(screen.queryByTestId("home-card-receipt")).toBeNull();
    expect(screen.getByRole("button", { name: "允许一次" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "拒绝" })).toBeEnabled();
  });

  it("treats a 409 HOST_OFFLINE as an inline error, not as handled elsewhere", () => {
    expect(answeredElsewhere(new HubHttpError(409, "HOST_OFFLINE", "offline"))).toBe(false);
    expect(answeredElsewhere(new HubHttpError(409, "INTERACTION_SUPERSEDED", "x"))).toBe(true);
    expect(answeredElsewhere(new HubHttpError(410, "INTERACTION_EXPIRED", "x"))).toBe(true);
    expect(answeredElsewhere(new Error("network"))).toBe(false);
  });

  it("greys every card button and says 数据可能过期 while stale", () => {
    oneApproval();
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 1 };
    renderQueue();
    expect(screen.getByRole("button", { name: "允许一次" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "拒绝" })).toBeDisabled();
    expect(screen.getByTestId("home-card-stale")).toHaveTextContent("数据可能过期");
  });
});

describe("HomeQueue push landing (?focus=, §9)", () => {
  it("rings the focused card and scrolls it into view", () => {
    fake.state = fiveCards();
    renderQueue("phone", "/m?focus=int_2");
    const card = cards().find((node) => node.getAttribute("data-card-id") === "int_2")!;
    expect(card).toHaveAttribute("data-focus", "true");
    expect(card.className).toMatch(/cardFocus/);
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
    expect(cards().filter((node) => node.getAttribute("data-focus") === "true")).toHaveLength(1);
  });

  it("expands the queue in place when the focused card sits behind the fold", () => {
    fake.state = fiveCards();
    renderQueue("phone", "/sessions?focus=int_4");
    expect(cards()).toHaveLength(4);
    const card = cards()[3];
    expect(card).toHaveAttribute("data-card-id", "int_4");
    expect(card).toHaveAttribute("data-focus", "true");
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  });
});
