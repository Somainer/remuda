import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { WakeUp } from "./wakes";

/**
 * ⑩ 「今天」 (home.md §2.2, §9): the ≥1440 right-column card. It is hidden
 * when there is no Main, no active wake-up, or no first read; a row opens the
 * shared sheet, and a stale read greys the card.
 */

const fake = vi.hoisted(() => ({
  main: null as unknown,
  wakes: [] as unknown[],
  loaded: true,
  freshness: { state: "fresh", lastOkAt: 1 as number | null, failingSince: null as number | null },
}));

vi.mock("../../lib/store", () => ({
  useMainEntry: () => fake.main,
}));

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
  HOME_FRESHNESS: { pollMs: 2_000, staleAfterMs: 10_000, bannerAfterMs: 5_000, receiptMs: 2_000 },
}));

vi.mock("./wakes", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./wakes")>();
  return {
    ...actual,
    useMainWakes: () => ({ lineageId: "lin_1", wakes: fake.wakes, loaded: fake.loaded }),
  };
});

import { TodayCard } from "./TodayCard";

function wake(patch: Partial<WakeUp> = {}): WakeUp {
  return {
    wakeId: "wak_1",
    lineageId: "lin_1",
    name: "每日汇总",
    text: "汇总",
    trigger: { kind: "every", everySecs: 86400, startAt: undefined },
    mode: "persistent",
    notify: "always",
    state: "active",
    nextFireAt: "2026-11-04T09:00:00Z",
    missedCount: 0,
    createdAt: "2026-11-01T00:00:00Z",
    updatedAt: "2026-11-01T00:00:00Z",
    creator: { origin: "agent" },
    ...patch,
  } as WakeUp;
}

function main() {
  return {
    instance: { id: "ins_main", hostId: "host-a", grants: ["address-owner"] },
    lineageId: "lin_1",
    generation: 1,
    live: true,
    chapterIds: ["ins_main"],
  };
}

describe("⑩ 「今天」 card", () => {
  it("renders nothing without a Main", () => {
    fake.main = null;
    fake.wakes = [wake()];
    const { container } = render(<TodayCard />);
    expect(container.querySelector("[data-testid='home-today']")).toBeNull();
  });

  it("renders nothing before the first read", () => {
    fake.main = main();
    fake.wakes = [wake()];
    fake.loaded = false;
    const { container } = render(<TodayCard />);
    expect(container.querySelector("[data-testid='home-today']")).toBeNull();
    fake.loaded = true;
  });

  it("hides itself when no wake-up can still fire", () => {
    fake.main = main();
    fake.wakes = [wake({ state: "done", nextFireAt: undefined })];
    const { container } = render(<TodayCard />);
    expect(container.querySelector("[data-testid='home-today']")).toBeNull();
  });

  it("lists the rows and opens the sheet on a tap", () => {
    fake.main = main();
    fake.wakes = [wake()];
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    render(<TodayCard />);
    expect(screen.getByTestId("home-today-title")).toHaveTextContent("今天");
    expect(screen.getByTestId("home-today-count")).toHaveTextContent("共 1 个");
    const row = screen.getByTestId("home-today-row");
    expect(row).toHaveTextContent("每日汇总");
    expect(row).toHaveTextContent(/^\d{2}:\d{2}/);
    expect(screen.queryByTestId("wakes-sheet")).toBeNull();
    fireEvent.click(row);
    expect(screen.getByTestId("wakes-sheet")).toBeInTheDocument();
    expect(screen.getByTestId("wakes-cancel")).toBeInTheDocument();
  });

  it("greys the card while stale but still opens the sheet", () => {
    fake.main = main();
    fake.wakes = [wake()];
    fake.freshness = { state: "stale", lastOkAt: 1, failingSince: 2 };
    render(<TodayCard />);
    expect(screen.getByTestId("home-today")).toHaveAttribute("data-stale", "1");
    fireEvent.click(screen.getByTestId("home-today-row"));
    // The sheet's cancel is held while stale (§8).
    expect(screen.getByTestId("wakes-cancel")).toBeDisabled();
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
  });
});
