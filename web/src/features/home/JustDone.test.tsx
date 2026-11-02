import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { HubState } from "../../lib/store";

/**
 * ⑤ 刚完成 (home.md §7, §9): hidden before the snapshot, while stale and while
 * empty; 打开 marks the row seen; 全部看过 writes once per listed row.
 */

const markSeen = vi.hoisted(() => vi.fn().mockResolvedValue(null));

const fake = vi.hoisted(() => ({
  state: null as unknown,
  freshness: { state: "fresh", lastOkAt: null as number | null, failingSince: null as number | null },
}));

vi.mock("../../lib/store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/store")>();
  return {
    ...actual,
    useHub: () => fake.state,
    hubStore: { ...actual.hubStore, titleOf: (id: string) => `title ${id}`, markSeen },
  };
});

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
}));

import { JustDone } from "./JustDone";

function hub(instances: unknown[]): HubState {
  return { instances, interactions: [], hosts: [], workspaces: [], answering: {} } as unknown as HubState;
}

/** Idle and unseen: rank done-unseen. */
function row(id: string, minute: number, extra: Record<string, unknown> = {}) {
  return {
    id,
    updatedAt: `2026-10-31T10:${String(minute).padStart(2, "0")}:00.000Z`,
    createdAt: "2026-10-31T08:00:00.000Z",
    lifecycle: "running",
    activity: { state: "known", value: "idle" },
    connectivity: "connected",
    parent: null,
    unseen: true,
    durableSeq: "1",
    ...extra,
  };
}

function renderSection(placement: "phone" | "desktop" = "phone") {
  return render(
    <MemoryRouter>
      <JustDone placement={placement} />
    </MemoryRouter>,
  );
}

describe("JustDone", () => {
  it("renders nothing before the first snapshot and while stale", () => {
    fake.state = hub([row("ins_a", 1)]);
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    const { unmount } = renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
    unmount();
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 1 };
    renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
  });

  it("renders nothing when no row is ◉", () => {
    fake.state = hub([row("ins_a", 1, { unseen: false })]);
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
  });

  it("shows three rows then +N, and expands the rest", async () => {
    fake.state = hub([1, 2, 3, 4, 5].map((minute) => row(`ins_${minute}`, minute)));
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderSection();
    const section = screen.getByTestId("home-just-done");
    expect(section).toHaveTextContent("刚完成 · 5");
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(3);
    expect(screen.getByTestId("just-done-more")).toHaveTextContent("+2");
    await userEvent.click(screen.getByTestId("just-done-more"));
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(5);
    expect(screen.queryByTestId("just-done-more")).toBeNull();
  });

  it("marks the row seen when 打开 is clicked", async () => {
    markSeen.mockClear();
    fake.state = hub([row("ins_a", 1)]);
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderSection();
    await userEvent.click(screen.getByTestId("just-done-open"));
    expect(markSeen).toHaveBeenCalledWith("ins_a");
  });

  it("writes once per listed row on 全部看过, including the rows behind +N", async () => {
    markSeen.mockClear();
    fake.state = hub([1, 2, 3, 4].map((minute) => row(`ins_${minute}`, minute)));
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderSection();
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(3);
    await userEvent.click(screen.getByTestId("just-done-mark-all"));
    expect(markSeen.mock.calls.map((call) => call[0]).sort()).toEqual(["ins_1", "ins_2", "ins_3", "ins_4"]);
  });

  it("lists top-level sessions only on the desktop", () => {
    fake.state = hub([
      row("ins_parent", 1),
      row("ins_child", 2, { parent: { instanceId: "ins_parent", runId: "run_1", commandId: "cmd_1" } }),
    ]);
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderSection("desktop");
    expect(screen.getByTestId("home-just-done")).toHaveTextContent("刚完成 · 1");
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(1);
  });
});
