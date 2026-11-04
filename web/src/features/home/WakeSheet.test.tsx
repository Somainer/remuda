import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WakeSheet } from "./WakeSheet";
import type { WakeUp } from "./wakes";

/**
 * ⑩'s sheet (home.md §6.2, §9): each wake-up with its trigger, next fire and
 * notify, and a cancel behind a confirm that uses the Hub's DELETE. A stale
 * read greys the cancel without hiding the list.
 */

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

function renderSheet(wakes: WakeUp[], stale = false) {
  const cancel = vi.fn().mockResolvedValue(undefined);
  const onClose = vi.fn();
  render(
    <WakeSheet open wakes={wakes} stale={stale} onClose={onClose} store={{ cancel }} />,
  );
  return { cancel, onClose };
}

describe("⑩ the wake sheet", () => {
  it("lists name, trigger, next fire and notify", () => {
    renderSheet([wake()]);
    const row = screen.getByTestId("wakes-row");
    expect(row).toHaveTextContent("每日汇总");
    expect(row).toHaveTextContent("每 1 天");
    expect(row).toHaveTextContent(/下次 \d{2}:\d{2}/);
    expect(row).toHaveTextContent("总是通知");
  });

  it("cancels only behind a confirm, through the store's DELETE", async () => {
    const { cancel } = renderSheet([wake()]);
    fireEvent.click(screen.getByTestId("wakes-cancel"));
    // Nothing has been removed yet.
    expect(screen.getByTestId("wake-cancel-confirm")).toHaveTextContent("取消定时「每日汇总」");
    expect(cancel).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("wake-cancel-yes"));
    await vi.waitFor(() => expect(cancel).toHaveBeenCalledWith("wak_1"));
    await vi.waitFor(() => expect(screen.queryByTestId("wake-cancel-confirm")).toBeNull());
  });

  it("返回 closes the confirm and cancels nothing", () => {
    const { cancel } = renderSheet([wake()]);
    fireEvent.click(screen.getByTestId("wakes-cancel"));
    fireEvent.click(screen.getByTestId("wake-cancel-no"));
    expect(screen.queryByTestId("wake-cancel-confirm")).toBeNull();
    expect(cancel).not.toHaveBeenCalled();
  });

  it("greys the cancel controls while stale but still lists the rows", () => {
    renderSheet([wake()], true);
    expect(screen.getByTestId("wakes-row")).toHaveTextContent("每日汇总");
    expect(screen.getByTestId("wakes-cancel")).toBeDisabled();
  });

  it("a done wake-up is marked and offers no cancel", () => {
    renderSheet([wake({ state: "done", nextFireAt: undefined })]);
    expect(screen.getByTestId("wakes-row-state")).toHaveTextContent("已完成");
    expect(screen.getByTestId("wakes-cancel")).toBeDisabled();
  });
});
