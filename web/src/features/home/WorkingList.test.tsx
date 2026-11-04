import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { OwnerGlance, OwnerGlanceRow } from "../../types/generated";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { WorkingList } from "./WorkingList";

const hub = {
  ready: true,
  connection: "live",
  instances: [] as Instance[],
  glance: null as OwnerGlance | null,
  glanceAt: null as number | null,
  glanceFailed: false,
};

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
  hubStore: {
    titleOf: (id: string) => `title ${id}`,
    hostName: () => "alpha",
    canInterrupt: true,
    cancel: vi.fn().mockResolvedValue(undefined),
    markSeen: vi.fn().mockResolvedValue(null),
    close: vi.fn(),
    rename: vi.fn(),
    toast: vi.fn(),
  },
}));

function row(id: string, minute: number, activity: "working" | "idle" = "working"): Instance {
  return {
    ...mockDb.instances[0],
    id: id as Id,
    lifecycle: "running",
    connectivity: "connected",
    activity: known(activity),
    parent: null,
    updatedAt: `2026-10-30T10:${String(minute).padStart(2, "0")}:00Z`,
  } as Instance;
}

/** The owner glance's working page, as the Hub orders it (newest first). */
function glanceOf(ids: string[], working = ids.length): OwnerGlance {
  const rows: OwnerGlanceRow[] = ids.map((id) => ({
    instanceId: id,
    kind: "working",
    headline: `headline ${id}`,
    title: `title ${id}`,
  }));
  return {
    generatedAt: "2026-10-30T10:09:00.000Z",
    timezoneOffsetMinutes: 0,
    counts: { needYou: 0, doneUnseen: 0, working, doneToday: 0 },
    doneUnseenIds: [],
    waiting: [],
    working: rows,
    more: { waiting: 0, working: Math.max(0, working - ids.length) },
  } as OwnerGlance;
}

function renderList() {
  return render(
    <MemoryRouter>
      <WorkingList />
    </MemoryRouter>,
  );
}

describe("WorkingList (home.md ⑥)", () => {
  // Each case sets its own world; freshness starts clean.
  beforeEach(() => {
    hub.glanceAt = null;
    hub.glanceFailed = false;
  });
  it("lists the glance's working rows as served, five rows then +N ›", () => {
    hub.instances = [1, 2, 3, 4, 5, 6, 7].map((m) => row(`ins_${m}`, m));
    hub.glance = glanceOf(["ins_7", "ins_6", "ins_5", "ins_4", "ins_3", "ins_2", "ins_1"]);
    renderList();
    const rows = screen.getAllByTestId("home-working-row");
    expect(rows).toHaveLength(5);
    expect(rows[0].textContent).toContain("title ins_7");
    expect(screen.getByTestId("home-working-overflow").textContent).toBe("+2 ›");
    expect(screen.getByText("工作中 · 7")).toBeTruthy();
  });

  it("says so when the glance reports nothing running", () => {
    hub.instances = [row("ins_idle", 1, "idle")];
    hub.glance = glanceOf([]);
    renderList();
    expect(screen.getByText("没有在跑的会话")).toBeTruthy();
  });

  it("greys the list when only the glance failed (frozen rows are not fresh)", () => {
    hub.instances = [row("ins_a", 1)];
    hub.glance = glanceOf(["ins_a"], 1);
    hub.glanceAt = Date.now();
    hub.glanceFailed = true;
    renderList();
    expect(screen.getByTestId("home-working")).toHaveAttribute("data-stale", "1");
  });

  it("never says nothing runs under a non-zero count", () => {
    hub.instances = [];
    // The count says two sessions run, but their store rows have not arrived.
    hub.glance = glanceOf(["ins_missing_a", "ins_missing_b"], 2);
    renderList();
    expect(screen.getByText("工作中 · 2")).toBeTruthy();
    expect(screen.queryByText("没有在跑的会话")).toBeNull();
    expect(screen.getByTestId("home-working-pending")).toBeTruthy();
  });

  it("is a skeleton with 「工作中 · –」 before the first glance, never an empty state", () => {
    hub.instances = [row("ins_a", 1)];
    hub.glance = null;
    renderList();
    expect(screen.getByText("工作中 · –")).toBeTruthy();
    expect(screen.queryByText("没有在跑的会话")).toBeNull();
    expect(screen.queryByTestId("home-working-row")).toBeNull();
  });

  it("releases the menu id when its row drops behind +N, so it cannot re-open by itself", async () => {
    const instances = [1, 2, 3, 4, 5, 6, 7, 8].map((m) => row(`ins_${m}`, m));
    hub.instances = instances;
    hub.glance = glanceOf(["ins_7", "ins_6", "ins_5", "ins_4", "ins_3"]);
    const { rerender } = renderList();
    const view = () => (
      <MemoryRouter>
        <WorkingList />
      </MemoryRouter>
    );
    const fifth = screen.getAllByTestId("home-working-row")[4];
    expect(fifth.textContent).toContain("title ins_3");
    await userEvent.click(fifth.querySelector("[data-testid='row-actions-more']") as HTMLElement);
    expect(screen.getByTestId("row-actions-menu")).toBeTruthy();

    // A newer session pushes ins_3 behind 「+N ›」: its menu is not painted.
    hub.glance = glanceOf(["ins_8", "ins_7", "ins_6", "ins_5", "ins_4", "ins_3"]);
    rerender(view());
    expect(screen.getAllByTestId("home-working-row")).toHaveLength(5);
    expect(screen.queryByTestId("row-actions-menu")).toBeNull();

    // Back inside the painted five: the stale id must not pop the menu open.
    hub.glance = glanceOf(["ins_7", "ins_6", "ins_5", "ins_4", "ins_3"]);
    rerender(view());
    expect(screen.getByText("title ins_3")).toBeTruthy();
    expect(screen.queryByTestId("row-actions-menu")).toBeNull();
  });
});
