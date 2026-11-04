import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { OwnerGlance } from "../../types/generated";
import type { HubState } from "../../lib/store";

/**
 * ① 的渲染契约 (home.md §5.2 / §5.3 / §8): the row's grammar is unit-tested
 * purely in glance.test.ts; this file pins the wiring — every count comes from
 * the ONE owner glance (never from the instance list), first snapshot 「–」,
 * the desktop-only 今日完成 segment, the stale grey + 「更新于 HH:MM」, and the
 * two taps.
 */

const fake = vi.hoisted(() => ({
  state: null as unknown,
  freshness: { state: "fresh", lastOkAt: null as number | null, failingSince: null as number | null },
}));

vi.mock("../../lib/store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/store")>();
  return {
    ...actual,
    useHub: () => fake.state,
    hubStore: {
      ...actual.hubStore,
      titleOf: (id: string) => id,
      getSnapshot: () => fake.state,
      // The ① row owns the home's glance poll (§5.3); the unit tests drive
      // the store through the fake state instead of a network tick.
      refreshGlance: vi.fn(() => Promise.resolve(undefined)),
    },
  };
});

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
  HOME_FRESHNESS: { pollMs: 2_000, staleAfterMs: 10_000, bannerAfterMs: 5_000, receiptMs: 2_000 },
}));

import { HomeGlance } from "./HomeGlance";

function glance(overrides: Partial<OwnerGlance["counts"]> = {}): OwnerGlance {
  return {
    generatedAt: "2026-10-31T08:00:00.000Z",
    timezoneOffsetMinutes: 480,
    counts: { needYou: 0, doneUnseen: 0, working: 0, doneToday: 0, ...overrides },
    doneUnseenIds: [],
    waiting: [],
    working: [],
    more: { waiting: 0, working: 0 },
  } as OwnerGlance;
}

function hub(overrides: Partial<HubState> = {}): HubState {
  return {
    instances: [],
    interactions: [],
    hosts: [],
    workspaces: [],
    answering: {},
    glance: null,
    glanceAt: null,
    glanceFailed: false,
    ...overrides,
  } as unknown as HubState;
}

const noop = () => {};

describe("HomeGlance", () => {
  it("reads 「–」 for every segment before the first glance", () => {
    fake.state = hub();
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("glance-need-you")).toHaveTextContent("– 需要你");
    expect(screen.getByTestId("glance-done-unseen")).toHaveTextContent("– 完成未看");
    expect(screen.getByTestId("glance-working")).toHaveTextContent("– 工作中");
    expect(screen.queryByTestId("glance-greeting")).toBeNull();
    expect(screen.queryByTestId("glance-updated")).toBeNull();
  });

  it("paints the glance's counts, never a derivation over the instance list", () => {
    // Three instances that would derive a ● of 3 on their own: the glance is
    // the only source, so the segment must read its number instead.
    fake.state = hub({
      instances: [instance("ins_a"), instance("ins_b"), instance("ins_c")],
      glance: glance({ needYou: 2, doneUnseen: 1, working: 7 }),
    });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("glance-need-you")).toHaveTextContent("2 需要你");
    expect(screen.getByTestId("glance-done-unseen")).toHaveTextContent("1 完成未看");
    expect(screen.getByTestId("glance-working")).toHaveTextContent("7 工作中");
  });

  it("shows 「今日完成 N」 on the desktop only, and never while unknown", () => {
    fake.state = hub({ glance: glance({ doneToday: 3 }) });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    const { unmount } = render(<HomeGlance placement="desktop" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("glance-done-today")).toHaveTextContent("今日完成 3");
    unmount();
    // The phone has no room for it (§5.2).
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.queryByTestId("glance-done-today")).toBeNull();
  });

  it("hides 今日完成 when the Hub could not say (no glance yet)", () => {
    fake.state = hub();
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    render(<HomeGlance placement="desktop" onNeedYou={noop} onWorking={noop} />);
    expect(screen.queryByTestId("glance-done-today")).toBeNull();
  });

  it("greys the whole row and stamps 更新于 HH:MM while stale", () => {
    fake.state = hub();
    fake.freshness = { state: "stale", lastOkAt: new Date(2026, 9, 31, 9, 5).getTime(), failingSince: 1 };
    const { container } = render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("home-glance")).toHaveAttribute("data-stale", "1");
    expect(screen.getByTestId("glance-updated")).toHaveTextContent("更新于 09:05");
    expect(container.querySelector('[data-stale="1"]')).toBeTruthy();
  });

  it("greys the row and names the last good read when only the glance failed", () => {
    const lastGood = new Date(2026, 9, 31, 9, 5).getTime();
    fake.state = hub(
      { glance: glance({ needYou: 2 }), glanceAt: lastGood, glanceFailed: true },
    );
    fake.freshness = { state: "fresh", lastOkAt: Date.now(), failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    // The main refresh is healthy, but these numbers are frozen: never fresh.
    expect(screen.getByTestId("home-glance")).toHaveAttribute("data-stale", "1");
    expect(screen.getByTestId("glance-updated")).toHaveTextContent("更新于 09:05");
    expect(screen.getByTestId("glance-need-you")).toHaveTextContent("2 需要你");
  });

  it("greys the row when the last good read is past the §8 threshold", () => {
    fake.state = hub({ glance: glance({ needYou: 1 }), glanceAt: Date.now() - 60_000 });
    fake.freshness = { state: "fresh", lastOkAt: Date.now(), failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("home-glance")).toHaveAttribute("data-stale", "1");
    expect(screen.getByTestId("glance-updated")).toBeTruthy();
  });

  it("never shows the stamp for a foreground grace", () => {
    fake.state = hub();
    fake.freshness = { state: "grace", lastOkAt: 1, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.queryByTestId("glance-updated")).toBeNull();
    expect(screen.getByTestId("home-glance")).not.toHaveAttribute("data-stale");
  });

  it("shows the empty-queue greeting with the hosts tail when nothing needs the owner", () => {
    fake.state = hub({
      glance: glance(),
      hosts: [{ id: "hst_1", label: "forge", state: "offline" } as HubState["hosts"][number]],
    });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    const greeting = screen.getByTestId("glance-greeting");
    expect(greeting).toHaveTextContent(/都处理完了/);
    expect(screen.getByTestId("glance-hosts")).toHaveTextContent("0/1 主机在线");
    expect(screen.queryByTestId("glance-need-you")).toBeNull();
    expect(screen.queryByTestId("glance-working")).toBeNull();
  });

  it("keeps a zero 工作中 segment clickable while its filter is active", async () => {
    fake.state = hub({ glance: glance() });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    const onWorking = vi.fn();
    const user = userEvent.setup();
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={onWorking} workingFilter />);
    const working = screen.getByTestId("glance-working");
    expect(working).toHaveTextContent("0 工作中");
    expect(working).toHaveAttribute("aria-pressed", "true");
    await user.click(working);
    expect(onWorking).toHaveBeenCalledTimes(1);
  });

  it("toggles the 完成未看 filter and reports its pressed state", async () => {
    fake.state = hub({ glance: glance({ doneUnseen: 1 }) });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    const onDoneUnseen = vi.fn();
    const user = userEvent.setup();
    render(
      <HomeGlance placement="desktop" onNeedYou={noop} onWorking={noop} onDoneUnseen={onDoneUnseen} doneUnseenFilter />,
    );
    const segment = screen.getByTestId("glance-done-unseen");
    expect(segment).toHaveTextContent("1 完成未看");
    expect(segment).toHaveAttribute("aria-pressed", "true");
    await user.click(segment);
    expect(onDoneUnseen).toHaveBeenCalledTimes(1);
  });

  it("toggles the working filter segment and reports its state", async () => {
    fake.state = hub({ glance: glance({ working: 1 }) });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    const onWorking = vi.fn();
    const user = userEvent.setup();
    render(
      <HomeGlance placement="desktop" onNeedYou={noop} onWorking={onWorking} workingFilter />,
    );
    const working = screen.getByTestId("glance-working");
    expect(working).toHaveTextContent("1 工作中");
    expect(working).toHaveAttribute("aria-pressed", "true");
    await user.click(working);
    expect(onWorking).toHaveBeenCalledTimes(1);
  });
});

/** A live instance on an unknown activity: rank 3 (home.md §3.1 h). */
function instance(id = "ins_a", parent: unknown = null, extra: Record<string, unknown> = {}) {
  return {
    id,
    revision: "1",
    createdAt: "2026-10-31T08:00:00.000Z",
    updatedAt: "2026-10-31T08:00:00.000Z",
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    driver: "claude-cli",
    lifecycle: "running",
    activity: { state: "unknown" },
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {},
    processRef: {},
    specRevision: "1",
    launchId: { state: "unknown" },
    capabilities: {},
    ownerFence: "1",
    activeRunIds: [],
    parent,
    journalId: "jrn_1",
    durableSeq: "1",
    exit: { state: "na" },
    ...extra,
  } as unknown as HubState["instances"][number];
}
