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
  notice,
  question,
  task,
} from "./queueTestData";
import type { OwnerNotice } from "./notices";

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
  notices: [] as OwnerNotice[],
  ack: vi.fn(),
  main: null as unknown,
  send: vi.fn(),
}));

vi.mock("../../lib/store", () => ({
  useHub: () => fake.state,
  useMainEntry: () => fake.main,
  hubStore: {
    titleOf: (id: string) => id,
    hostName: () => "forge",
    getSnapshot: () => fake.state,
    respond: fake.respond,
    refresh: fake.refresh,
    markSeen: fake.markSeen,
    send: fake.send,
  },
}));

vi.mock("./notices", () => ({
  useOwnerNotices: () => ({ notices: fake.notices, loaded: true }),
  noticeStore: { ack: fake.ack },
  noticeVoice: (row: { origin: string }) => (row.origin === "hub" ? "Hub" : row.origin === "human" ? "备忘" : "Main"),
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
  fake.notices = [];
  fake.ack.mockReset();
  fake.main = null;
  fake.send.mockReset().mockResolvedValue(true);
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

  it("pins Main's own question first, labelled, and counts it once (§4.1 item 1)", () => {
    fake.main = {
      instance: { id: "ins_main", hostId: "hst_1", grants: ["address-owner"] },
      lineageId: "ins_main",
      generation: 1,
      live: true,
      chapterIds: ["ins_main"],
    };
    fake.state = hub({
      instances: [inst({ id: "ins_main" }), inst({ id: "ins_a" })],
      interactions: [
        // Newer, but not Main's: it comes second.
        interaction({ id: "int_other", instanceId: "ins_a", createdAt: T2, request: question() }),
        // Older, Main's own: pinned to the front and labelled.
        interaction({ id: "int_main", instanceId: "ins_main", createdAt: T0, request: question() }),
      ],
    });
    renderQueue("phone");
    expect(cards().map((card) => card.getAttribute("data-card-id"))).toEqual(["int_main", "int_other"]);
    expect(cards()[0]).toHaveTextContent("Main 在问你");
    expect(cards()[1]).not.toHaveTextContent("Main 在问你");
    // The pinned card is one of the same queue rows: N = 2, never 3.
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 2");
  });

  it("hands a question or plan card to Main from its ⋯ menu and leaves it pending (§6.4)", async () => {
    fake.main = {
      instance: { id: "ins_main", hostId: "hst_1", grants: ["address-owner"] },
      lineageId: "ins_main",
      generation: 1,
      live: true,
      chapterIds: ["ins_main"],
    };
    fake.state = hub({
      instances: [inst({ id: "ins_a" }), inst({ id: "ins_p" }), inst({ id: "ins_approval" })],
      interactions: [
        interaction({ id: "int_ask", instanceId: "ins_a", createdAt: T2, request: question() }),
        interaction({ id: "int_plan", instanceId: "ins_p", createdAt: T1, request: PLAN }),
        interaction({ id: "int_approval", instanceId: "ins_approval", createdAt: T0 }),
      ],
    });
    renderQueue("phone");
    // An approval card carries no ⋯ at all.
    const approval = cards().find((card) => card.getAttribute("data-card-id") === "int_approval")!;
    expect(within(approval).queryByTestId("home-card-more")).toBeNull();

    const ask = cards().find((card) => card.getAttribute("data-card-id") === "int_ask")!;
    fireEvent.click(within(ask).getByTestId("home-card-more"));
    fireEvent.click(screen.getByTestId("home-card-hand-main"));
    await act(async () => {
      await Promise.resolve();
    });
    expect(fake.send).toHaveBeenCalledTimes(1);
    const [target, text] = fake.send.mock.calls[0] as [string, string];
    expect(target).toBe("ins_main");
    expect(text).toContain("untrusted: ");
    expect(text).toContain("旧迁移 0042 删掉还是保留？");
    // Sending never answers: no interaction POST was made.
    expect(fake.respond).not.toHaveBeenCalled();
    // The card is still in the queue, pending.
    expect(cards().some((card) => card.getAttribute("data-card-id") === "int_ask")).toBe(true);
  });

  it("hides 交给 Main when no Main resolves", () => {
    fake.main = null;
    fake.state = hub({
      instances: [inst({ id: "ins_a" })],
      interactions: [interaction({ id: "int_ask", instanceId: "ins_a", request: question() })],
    });
    renderQueue("phone");
    const card = cards()[0];
    expect(within(card).queryByTestId("home-card-more")).toBeNull();
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

describe("HomeQueue 告诉你 (§4.1 kind 4, §4.2, D-060 §11.5)", () => {
  it("renders Main's notice after 任务待你 with title, body, 知道了 and 打开对话, outside the count", () => {
    fake.tasks = [task({ id: "tsk_b", blockedReason: "等你确认" })];
    fake.notices = [notice({ noticeId: "ntc_1" })];
    fake.state = hub({
      instances: [inst({ id: "ins_f", lastTurnError: { at: T1, text: "API Error: 529" }, unseen: true })],
    });
    renderQueue("desktop", "/sessions");
    expect(cards().map((card) => card.getAttribute("data-card-type"))).toEqual(["task", "notice", "failed"]);
    // N counts 任务待你 only (§5.1): not the notice, not the failed turn.
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 1");
    const card = cards()[1];
    expect(card).toHaveAttribute("data-notice-id", "ntc_1");
    // Main's voice, not the owner's.
    expect(within(card).getByText("告诉你 · Main")).toBeInTheDocument();
    expect(within(card).getByTestId("home-card-title")).toHaveTextContent("Nightly gate red");
    expect(within(card).getByTestId("home-card-body")).toHaveTextContent("Lane 2 failed: wire_golden.");
    expect(within(card).getByTestId("home-card-open")).toHaveAttribute("href", "/s/ins_main");
  });

  it("acknowledges on the Hub with 知道了 and shows a failure inline", async () => {
    fake.notices = [notice({ noticeId: "ntc_1" }), notice({ noticeId: "ntc_2", origin: "human", chapterInstanceId: null, createdAt: T0 })];
    fake.state = hub();
    fake.ack.mockResolvedValueOnce(notice({ noticeId: "ntc_1", ackedAt: T2 }));
    fake.ack.mockRejectedValueOnce(new Error("hub down"));
    renderQueue();
    const [first, second] = cards();
    // The owner's own notice has no conversation to open.
    expect(within(second).queryByTestId("home-card-open")).toBeNull();
    expect(within(second).getByText("告诉你 · 备忘")).toBeInTheDocument();
    await act(async () => {
      fireEvent.click(within(first).getByTestId("home-card-ack"));
    });
    expect(fake.ack).toHaveBeenCalledWith("ntc_1");
    await act(async () => {
      fireEvent.click(within(second).getByTestId("home-card-ack"));
    });
    expect(within(second).getByRole("alert")).toHaveTextContent("hub down");
    expect(within(second).getByTestId("home-card-ack")).toBeEnabled();
  });

  it("leaves the queue once the acknowledged notice is gone from the list", () => {
    fake.notices = [notice({ noticeId: "ntc_1" })];
    fake.state = hub();
    const view = renderQueue();
    expect(cards()).toHaveLength(1);
    fake.notices = [];
    rerenderQueue(view);
    expect(cards()).toHaveLength(0);
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 0");
  });

  it("greys 知道了 with 数据可能过期 while the queue is stale (§8)", () => {
    fake.notices = [notice({ noticeId: "ntc_1" })];
    fake.state = hub();
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 1 };
    renderQueue();
    const card = cards()[0];
    expect(within(card).getByTestId("home-card-ack")).toBeDisabled();
    expect(within(card).getByTestId("home-card-stale")).toHaveTextContent("数据可能过期");
    expect(within(card).getByTestId("home-card-open")).not.toHaveAttribute("aria-disabled");
    fireEvent.click(within(card).getByTestId("home-card-ack"));
    expect(fake.ack).not.toHaveBeenCalled();
  });

  it("rings a notice a push landed on (?focus=<noticeId>)", () => {
    fake.notices = [notice({ noticeId: "ntc_1" })];
    fake.state = hub();
    renderQueue("phone", "/m?focus=ntc_1");
    expect(cards()[0]).toHaveAttribute("data-focus", "true");
  });
});

describe("HomeQueue holds the order only while interacting (§3.3, §4.1)", () => {
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

  /** The live poll: a newer interaction arrives at the head. */
  function withNewest(ids: string[]): HubState {
    const created = ["2026-10-31T11:00:00.000Z", T2, T1, T0, "2026-10-31T07:00:00.000Z"];
    return hub({
      instances: ids.map((id) => inst({ id: `ins_${id}` })),
      interactions: ids.map((id, index) =>
        interaction({ id, instanceId: `ins_${id}`, createdAt: created[index] }),
      ),
    });
  }

  function paintedIds() {
    return cards().map((card) => card.getAttribute("data-card-id"));
  }

  it("(a) does not hold an expanded queue left idle: a new card paints", () => {
    fake.state = fiveInteractions();
    const view = renderQueue("phone");
    fireEvent.click(screen.getByTestId("home-queue-more"));
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
    expect(paintedIds()).toEqual(["int_1", "int_2", "int_3", "int_4", "int_5"]);

    // §3.3 freezes only while interacting; an open queue nobody touches stays
    // live, so the poll's newest card lands at the head.
    fake.state = withNewest(["int_0", "int_1", "int_2", "int_3", "int_4"]);
    rerenderQueue(view);
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
    expect(paintedIds()).toEqual(["int_0", "int_1", "int_2", "int_3", "int_4"]);
  });

  it("(b) holds the order while a finger is on a card, and re-orders on release", () => {
    fake.state = fiveInteractions();
    const view = renderQueue("phone");
    fireEvent.click(screen.getByTestId("home-queue-more"));
    act(() => {
      cards()[0].dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "1");

    // The poll lands a newer card and drops one while the finger is down.
    const next = withNewest(["int_0", "int_1", "int_2", "int_3", "int_4"]);
    fake.state = hub({
      instances: next.instances,
      interactions: [
        interaction({ id: "int_0", instanceId: "ins_int_0", createdAt: "2026-10-31T11:00:00.000Z" }),
        interaction({ id: "int_1", instanceId: "ins_int_1", createdAt: T2 }),
        interaction({ id: "int_2", instanceId: "ins_int_2", createdAt: T1 }),
        interaction({ id: "int_3", instanceId: "ins_int_3", createdAt: T0 }),
        interaction({ id: "int_4", instanceId: "ins_int_4", createdAt: "2026-10-31T07:00:00.000Z" }),
      ],
    });
    rerenderQueue(view);
    expect(paintedIds()).toEqual(["int_1", "int_2", "int_3", "int_4", "int_5"]);
    // The card that vanished from the live data is an inert placeholder.
    expect(cards()[4]).toHaveAttribute("data-stale", "1");
    // The new card waits for the release.
    expect(paintedIds()).not.toContain("int_0");

    // Lifting the finger re-orders once.
    act(() => {
      fireEvent.pointerUp(document.body);
    });
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
    expect(paintedIds()).toEqual(["int_0", "int_1", "int_2", "int_3", "int_4"]);
  });

  it("(c) never strands a hold: answer every card, then a new one paints", async () => {
    vi.useFakeTimers();
    fake.respond.mockResolvedValue(undefined);
    fake.state = hub({
      instances: [inst({ id: "ins_a" })],
      interactions: [interaction({ id: "int_1", instanceId: "ins_a", createdAt: T2 })],
    });
    const view = renderQueue("phone");
    act(() => {
      cards()[0].dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "允许一次" }));
    });
    await act(async () => {
      vi.advanceTimersByTime(2_000);
    });
    // Every held card is answered: the painted list is empty, and the hold is
    // released rather than freezing an empty snapshot. The header reads the
    // LIVE count — the list still carries the answered row until its poll.
    expect(cards()).toHaveLength(0);
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-empty", "1");
    expect(screen.getByTestId("home-queue")).toHaveAttribute("data-order-held", "0");
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 1");

    // The poll drops the answered row: the header follows it to zero.
    fake.state = hub({ instances: [inst({ id: "ins_a" })], interactions: [] });
    rerenderQueue(view);
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 0");

    // The next poll's new card paints without any navigation.
    fake.state = hub({
      instances: [inst({ id: "ins_b" })],
      interactions: [interaction({ id: "int_2", instanceId: "ins_b", createdAt: "2026-10-31T11:00:00.000Z" })],
    });
    rerenderQueue(view);
    expect(paintedIds()).toEqual(["int_2"]);
    expect(screen.getByTestId("home-queue-title")).toHaveTextContent("需要你 · 1");
  });

  it("keeps a release control when answered cards shrink the expanded list", async () => {
    vi.useFakeTimers();
    fake.respond.mockResolvedValue(undefined);
    fake.state = fiveInteractions();
    renderQueue("phone");
    fireEvent.click(screen.getByTestId("home-queue-more"));
    expect(cards()).toHaveLength(5);

    // Answer three cards in place: only two remain and nothing is hidden any
    // more — the collapse row must still be there to close the expansion.
    for (let index = 0; index < 3; index += 1) {
      await act(async () => {
        fireEvent.click(screen.getAllByRole("button", { name: "允许一次" })[0]);
      });
      await act(async () => {
        vi.advanceTimersByTime(2_000);
      });
    }
    expect(cards()).toHaveLength(2);
    fireEvent.click(screen.getByTestId("home-queue-more"));
    expect(screen.queryByTestId("home-queue-more")).toBeNull();
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
