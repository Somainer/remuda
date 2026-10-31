import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { HubState } from "../../lib/store";

/**
 * ① 的渲染契约 (home.md §5.2 / §8): the row's grammar is unit-tested purely in
 * glance.test.ts; this file pins the wiring — first snapshot 「–」, the stale
 * grey + 「更新于 HH:MM」, and the two taps.
 */

const fake = vi.hoisted(() => ({
  state: null as unknown,
  freshness: { state: "fresh", lastOkAt: null as number | null, failingSince: null as number | null },
}));

vi.mock("../../lib/store", () => ({
  useHub: () => fake.state,
  hubStore: { titleOf: (id: string) => id, getSnapshot: () => fake.state },
}));

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
  HOME_FRESHNESS: { pollMs: 2_000, staleAfterMs: 10_000, bannerAfterMs: 5_000, receiptMs: 2_000 },
}));

vi.mock("../tasks/TaskList", () => ({
  useTaskLedger: () => ({ tasks: [], projectName: () => undefined }),
}));

import { HomeGlance } from "./HomeGlance";

function hub(overrides: Partial<HubState> = {}): HubState {
  return {
    instances: [],
    interactions: [],
    hosts: [],
    workspaces: [],
    answering: {},
    ...overrides,
  } as unknown as HubState;
}

const noop = () => {};

describe("HomeGlance", () => {
  it("reads 「–」 for both segments before the first snapshot", () => {
    fake.state = hub();
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("glance-need-you")).toHaveTextContent("– 需要你");
    expect(screen.getByTestId("glance-working")).toHaveTextContent("– 工作中");
    expect(screen.queryByTestId("glance-greeting")).toBeNull();
    expect(screen.queryByTestId("glance-updated")).toBeNull();
  });

  it("greys the whole row and stamps 更新于 HH:MM while stale", () => {
    fake.state = hub();
    fake.freshness = { state: "stale", lastOkAt: new Date(2026, 9, 31, 9, 5).getTime(), failingSince: 1 };
    const { container } = render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.getByTestId("home-glance")).toHaveAttribute("data-stale", "1");
    expect(screen.getByTestId("glance-updated")).toHaveTextContent("更新于 09:05");
    expect(container.querySelector('[data-stale="1"]')).toBeTruthy();
  });

  it("never shows the stamp for a foreground grace", () => {
    fake.state = hub();
    fake.freshness = { state: "grace", lastOkAt: 1, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    expect(screen.queryByTestId("glance-updated")).toBeNull();
    expect(screen.getByTestId("home-glance")).not.toHaveAttribute("data-stale");
  });

  it("shows the empty-queue greeting with the hosts tail when nothing needs the owner", () => {
    fake.state = hub({ hosts: [{ id: "hst_1", label: "forge", state: "offline" } as HubState["hosts"][number]] });
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    render(<HomeGlance placement="phone" onNeedYou={noop} onWorking={noop} />);
    const greeting = screen.getByTestId("glance-greeting");
    expect(greeting).toHaveTextContent(/都处理完了/);
    expect(screen.getByTestId("glance-hosts")).toHaveTextContent("0/1 主机在线");
    expect(screen.queryByTestId("glance-need-you")).toBeNull();
    expect(screen.queryByTestId("glance-working")).toBeNull();
  });

  it("keeps a zero 工作中 segment clickable while its filter is active", async () => {
    fake.state = hub();
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

  it("toggles the working filter segment and reports its state", async () => {
    fake.state = hub({ instances: [instance()] });
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
function instance() {
  return {
    id: "ins_a",
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
    parent: null,
    journalId: "jrn_1",
    durableSeq: "1",
    exit: { state: "na" },
  } as unknown as HubState["instances"][number];
}
