import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { WorkingList } from "./WorkingList";

const hub = { ready: true, connection: "live", instances: [] as Instance[] };

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
  hubStore: { titleOf: (id: string) => `title ${id}`, cancel: vi.fn().mockResolvedValue(undefined) },
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

function renderList() {
  return render(
    <MemoryRouter>
      <WorkingList />
    </MemoryRouter>,
  );
}

describe("WorkingList (home.md ⑥)", () => {
  it("lists working sessions newest first, five rows then +N ›", () => {
    hub.instances = [1, 2, 3, 4, 5, 6, 7].map((m) => row(`ins_${m}`, m)).concat(row("ins_idle", 9, "idle"));
    renderList();
    const rows = screen.getAllByTestId("home-working-row");
    expect(rows).toHaveLength(5);
    expect(rows[0].textContent).toContain("title ins_7");
    expect(screen.getByTestId("home-working-overflow").textContent).toBe("+2 ›");
    expect(screen.getByText("工作中 · 7")).toBeTruthy();
  });

  it("says so when nothing runs", () => {
    hub.instances = [row("ins_idle", 1, "idle")];
    renderList();
    expect(screen.getByText("没有在跑的会话")).toBeTruthy();
  });
});
