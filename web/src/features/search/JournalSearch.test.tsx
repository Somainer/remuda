import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

/**
 * cb-search: the record search under the session list's filter. It asks the
 * Hub once, on request, for the filter's words; each hit opens the events tab
 * at its seq; a changed filter clears stale hits; partial and indexing are
 * named.
 */

const rest = vi.hoisted(() => vi.fn());

vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, rest };
});

vi.mock("../../lib/store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/store")>();
  return { ...actual, hubStore: { ...actual.hubStore, titleOf: (id: string) => `title ${id}` } };
});

import { JournalSearch } from "./JournalSearch";
import { hitHref, hitKindLabel } from "./journalSearch";

function Where() {
  const location = useLocation();
  return <p data-testid="where">{location.pathname + location.search}</p>;
}

function renderAt(query: string) {
  return render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <Routes>
        <Route path="/sessions" element={<JournalSearch query={query} />} />
        <Route path="/s/:id/events" element={<Where />} />
      </Routes>
    </MemoryRouter>,
  );
}

const hit = {
  instanceId: "ins_worker",
  lineageId: "ins_seat",
  seq: "476",
  observedAt: "2026-11-04T10:19:18.330Z",
  kind: "message" as const,
  role: "assistant",
  matchedBy: "like" as const,
  snippet: "旧迁移 0042 先回滚",
};

describe("JournalSearch", () => {
  it("renders nothing without a filter and never asks on its own", () => {
    const { container } = renderAt("   ");
    expect(container.textContent).toBe("");
    renderAt("迁移");
    expect(screen.getByTestId("journal-search-run").textContent).toBe("搜索记录「迁移」");
    expect(rest).not.toHaveBeenCalled();
  });

  it("asks once on request and opens a hit at its seq", async () => {
    rest.mockReset().mockResolvedValue({ hits: [hit], partial: false, indexComplete: true });
    renderAt(" 迁移 ");
    await userEvent.click(screen.getByTestId("journal-search-run"));
    expect(rest).toHaveBeenCalledTimes(1);
    expect(rest.mock.calls[0][0]).toBe(`/v1/search?q=${encodeURIComponent("迁移")}&limit=50`);
    const link = await screen.findByTestId("journal-search-hit");
    expect(link.textContent).toContain("title ins_worker");
    expect(link.textContent).toContain("seq 476");
    expect(link.textContent).toContain("助手消息");
    expect(link.textContent).toContain("旧迁移 0042 先回滚");
    expect(screen.queryByTestId("journal-search-partial")).toBeNull();
    expect(screen.queryByTestId("journal-search-indexing")).toBeNull();
    await userEvent.click(link);
    expect(screen.getByTestId("where").textContent).toBe("/s/ins_worker/events?seq=476");
  });

  it("names an empty answer, a partial one and an index still filling", async () => {
    rest.mockReset().mockResolvedValue({ hits: [], partial: true, indexComplete: false });
    renderAt("nothing");
    await userEvent.click(screen.getByTestId("journal-search-run"));
    expect(await screen.findByTestId("journal-search-empty")).toBeTruthy();
    expect(screen.getByTestId("journal-search-partial")).toBeTruthy();
    expect(screen.getByTestId("journal-search-indexing")).toBeTruthy();
  });

  it("clears stale hits when the filter changes and reports a failure", async () => {
    rest.mockReset().mockResolvedValue({ hits: [hit], partial: false, indexComplete: true });
    const view = renderAt("迁移");
    await userEvent.click(screen.getByTestId("journal-search-run"));
    expect(await screen.findByTestId("journal-search-hit")).toBeTruthy();
    view.rerender(
      <MemoryRouter initialEntries={["/sessions"]}>
        <Routes>
          <Route path="/sessions" element={<JournalSearch query="回滚" />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.queryByTestId("journal-search-hit")).toBeNull();
    rest.mockReset().mockRejectedValue(new Error("down"));
    await act(async () => {
      await userEvent.click(screen.getByTestId("journal-search-run"));
    });
    expect(await screen.findByTestId("journal-search-failed")).toBeTruthy();
  });

  it("labels kinds and builds the events link", () => {
    expect(hitKindLabel({ kind: "tool_result" })).toBe("工具结果");
    expect(hitKindLabel({ kind: "message", role: "user" })).toBe("用户消息");
    expect(hitHref({ instanceId: "ins_a", seq: "12" })).toBe("/s/ins_a/events?seq=12");
  });
});
