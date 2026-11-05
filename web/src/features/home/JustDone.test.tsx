import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { HubState } from "../../lib/store";

/**
 * ⑤ 刚完成 (home.md §7, §9): hidden before the snapshot, while stale and while
 * empty; 打开 marks the row seen; 全部看过 writes once per listed row; and the
 * rows are EXACTLY the set ◉ counts, at every width.
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

vi.mock("./freshness", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./freshness")>();
  return { ...actual, useHomeFreshness: () => fake.freshness };
});

// The poll itself is the store's (useOwnerGlance.test); here the state is fixed.
vi.mock("./useOwnerGlance", () => ({ useOwnerGlancePoll: () => undefined }));

import { HomeGlance } from "./HomeGlance";
import { JustDone } from "./JustDone";

/** A Hub state whose owner glance counted `ids` as ◉ (newest first). */
function hub(ids: string[], extra: Partial<HubState> = {}, doneUnseen = ids.length): HubState {
  return {
    instances: [],
    interactions: [],
    hosts: [],
    workspaces: [],
    answering: {},
    glance: {
      generatedAt: "2026-10-31T10:00:00.000Z",
      timezoneOffsetMinutes: 0,
      counts: { needYou: 0, doneUnseen, working: 0, doneToday: 0 },
      doneUnseenIds: ids,
      waiting: [],
      working: [],
      more: { waiting: 0, working: 0 },
    },
    glanceAt: Date.now(),
    glanceFailed: false,
    ...extra,
  } as unknown as HubState;
}

function renderSection(placement: "phone" | "desktop" = "phone") {
  return render(
    <MemoryRouter>
      <JustDone placement={placement} />
    </MemoryRouter>,
  );
}

const FRESH = { state: "fresh", lastOkAt: 1, failingSince: null };

describe("JustDone", () => {
  it("renders nothing before the first snapshot, while stale, and while the glance is stale", () => {
    fake.state = hub(["ins_a"]);
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    const { unmount } = renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
    unmount();
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 1 };
    const second = renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
    second.unmount();
    // A failed glance read freezes its rows exactly as it greys ①.
    fake.freshness = FRESH;
    fake.state = hub(["ins_a"], { glanceFailed: true });
    const third = renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
    third.unmount();
    fake.state = hub(["ins_a"], { glance: null, glanceAt: null });
    renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
  });

  it("renders nothing when ◉ is empty", () => {
    fake.state = hub([]);
    fake.freshness = FRESH;
    renderSection();
    expect(screen.queryByTestId("home-just-done")).toBeNull();
  });

  it("shows three rows then +N, and expands the rest", async () => {
    fake.state = hub(["ins_5", "ins_4", "ins_3", "ins_2", "ins_1"]);
    fake.freshness = FRESH;
    renderSection();
    const section = screen.getByTestId("home-just-done");
    expect(section).toHaveTextContent("刚完成 · 5");
    expect(screen.getAllByTestId("just-done-row").map((row) => row.dataset.instanceId)).toEqual([
      "ins_5",
      "ins_4",
      "ins_3",
    ]);
    expect(screen.getByTestId("just-done-more")).toHaveTextContent("+2");
    await userEvent.click(screen.getByTestId("just-done-more"));
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(5);
    expect(screen.queryByTestId("just-done-more")).toBeNull();
    expect(screen.queryByTestId("just-done-unlisted")).toBeNull();
  });

  it("names the sessions past the Hub's row cap once expanded", async () => {
    fake.state = hub(["ins_4", "ins_3", "ins_2", "ins_1"], {}, 6);
    fake.freshness = FRESH;
    renderSection();
    expect(screen.getByTestId("home-just-done")).toHaveTextContent("刚完成 · 6");
    expect(screen.getByTestId("just-done-more")).toHaveTextContent("+3");
    await userEvent.click(screen.getByTestId("just-done-more"));
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(4);
    expect(screen.getByTestId("just-done-unlisted")).toHaveTextContent("+2");
  });

  it("marks the row seen when 打开 is clicked", async () => {
    markSeen.mockClear();
    fake.state = hub(["ins_a"]);
    fake.freshness = FRESH;
    renderSection();
    await userEvent.click(screen.getByTestId("just-done-open"));
    expect(markSeen).toHaveBeenCalledWith("ins_a");
  });

  it("writes once per listed row on 全部看过, including the rows behind +N", async () => {
    markSeen.mockClear();
    fake.state = hub(["ins_4", "ins_3", "ins_2", "ins_1"]);
    fake.freshness = FRESH;
    renderSection();
    expect(screen.getAllByTestId("just-done-row")).toHaveLength(3);
    await userEvent.click(screen.getByTestId("just-done-mark-all"));
    expect(markSeen.mock.calls.map((call) => call[0]).sort()).toEqual(["ins_1", "ins_2", "ins_3", "ins_4"]);
  });

  /**
   * [r4 item 3] The desktop used to drop child sessions from ⑤ while ◉
   * counted them, and both used to derive over EVERY instance rather than the
   * owner's world. Now both placements list exactly the glance's ◉ set — a
   * dispatched worker included — under the segment's own number.
   */
  it("lists exactly the set ◉ counts, at both widths", () => {
    const parent = {
      id: "ins_parent",
      lifecycle: "running",
      activity: { state: "known", value: "idle" },
      unseen: true,
      parent: null,
    };
    const child = {
      ...parent,
      id: "ins_child",
      parent: { instanceId: "ins_parent", runId: null, commandId: null },
      creatorOrigin: "agent",
    };
    // An unseen session OUTSIDE the owner scope (another agent's private
    // lineage): the instance list holds it, the glance did not count it.
    const outside = { ...parent, id: "ins_outside" };
    fake.freshness = FRESH;
    for (const placement of ["phone", "desktop"] as const) {
      fake.state = hub(["ins_child", "ins_parent"], { instances: [parent, child, outside] as never });
      const { unmount } = render(
        <MemoryRouter>
          <HomeGlance placement={placement} onNeedYou={() => undefined} onWorking={() => undefined} />
          <JustDone placement={placement} />
        </MemoryRouter>,
      );
      expect(screen.getByTestId("glance-done-unseen")).toHaveTextContent("2 完成未看");
      const section = screen.getByTestId("home-just-done");
      expect(section).toHaveTextContent("刚完成 · 2");
      expect(
        within(section)
          .getAllByTestId("just-done-row")
          .map((row) => row.dataset.instanceId),
      ).toEqual(["ins_child", "ins_parent"]);
      unmount();
    }
  });
});
