import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { HubHttpError } from "../lib/httpError";
import * as memoryApi from "../features/memory/api";
import type { MemoryEntry } from "../features/memory/api";
import { MemoryPage } from "./MemoryPage";

vi.mock("../lib/api", () => ({
  api: { instanceGet: vi.fn() },
}));

vi.mock("../features/memory/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../features/memory/api")>();
  return {
    ...actual,
    listMemory: vi.fn(),
    searchMemory: vi.fn(),
    getMemory: vi.fn(),
    createMemory: vi.fn(),
    updateMemory: vi.fn(),
    deleteMemory: vi.fn(),
    restoreMemory: vi.fn(),
    lineageExists: vi.fn(),
    listMemoryWrites: vi.fn(),
  };
});

const instanceGet = vi.mocked(api.instanceGet);
const lineageExists = vi.mocked(memoryApi.lineageExists);
const listMemory = vi.mocked(memoryApi.listMemory);
const searchMemory = vi.mocked(memoryApi.searchMemory);
const getMemory = vi.mocked(memoryApi.getMemory);
const createMemory = vi.mocked(memoryApi.createMemory);
const updateMemory = vi.mocked(memoryApi.updateMemory);
const deleteMemory = vi.mocked(memoryApi.deleteMemory);

function entry(overrides: Partial<MemoryEntry> & { slug: string }): MemoryEntry {
  return {
    lineageId: "ins_l",
    type: "user",
    title: overrides.slug,
    description: `about ${overrides.slug}`,
    body: "body",
    revision: 1,
    origin: "human",
    sourceRefs: [],
    turnRef: null,
    createdAt: "2026-10-30T00:00:00.000Z",
    updatedAt: "2026-10-30T00:00:00.000Z",
    ...overrides,
  };
}

function renderPage(initialEntry: string) {
  return render(
    <MemoryRouter initialEntries={[initialEntry]}>
      <Routes>
        <Route path="/s/:instanceId/memory" element={<MemoryPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  instanceGet.mockResolvedValue({ id: "ins_x", lineageId: "ins_l" } as never);
  lineageExists.mockResolvedValue(true);
  listMemory.mockResolvedValue([]);
  searchMemory.mockResolvedValue([]);
  getMemory.mockRejectedValue(new HubHttpError(404, "NOT_FOUND", "missing"));
  createMemory.mockResolvedValue({
    entry: entry({ slug: "about-me" }),
    entryBytes: 1,
    indexBytes: 1,
    warnings: [],
  });
  updateMemory.mockResolvedValue({
    entry: entry({ slug: "about-me", revision: 2 }),
    entryBytes: 1,
    indexBytes: 1,
    warnings: [],
  });
  deleteMemory.mockResolvedValue(undefined as never);
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("about-me is loaded on its own", () => {
  it("shows and edits the server copy even when the search filtered it out", async () => {
    // The search matches another entry; about-me is not in the list.
    searchMemory.mockResolvedValue([entry({ slug: "other", title: "别的" })]);
    getMemory.mockResolvedValue(
      entry({ slug: "about-me", body: "server about body", revision: 2 }),
    );

    renderPage("/s/ins_x/memory?q=%E8%BE%B9%E7%95%8C");

    const about = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(about.value).toBe("server about body"));
    await waitFor(() => expect(screen.getByTestId("memory-row")).toHaveAttribute("data-slug", "other"));

    fireEvent.click(screen.getByTestId("memory-about-save"));
    await waitFor(() =>
      expect(updateMemory).toHaveBeenCalledWith("ins_l", "about-me", 2, {
        body: "server about body",
      }),
    );
    expect(createMemory).not.toHaveBeenCalled();
  });
});
