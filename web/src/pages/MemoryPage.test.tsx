import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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

describe("an unsaved about-me draft survives reloads", () => {
  it("keeps the typed text when the list reloads under the same revision", async () => {
    const about = entry({ slug: "about-me", body: "saved body", revision: 1 });
    listMemory.mockResolvedValue([about, entry({ slug: "other" })]);
    getMemory.mockResolvedValue(about);

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(textarea.value).toBe("saved body"));
    fireEvent.change(textarea, { target: { value: "my unsaved draft" } });

    // A search reloads the list (and refreshes the standalone about copy).
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "边界" } });
    await waitFor(() => expect(searchMemory).toHaveBeenCalled());

    expect(textarea.value).toBe("my unsaved draft");
    expect(screen.queryByTestId("memory-conflict")).toBeNull();
    // A save after the reload still targets the loaded base revision.
    fireEvent.click(screen.getByTestId("memory-about-save"));
    await waitFor(() =>
      expect(updateMemory).toHaveBeenCalledWith("ins_l", "about-me", 1, {
        body: "my unsaved draft",
      }),
    );
  });

  it("keeps a brand-new about-me draft when the entry is still missing", async () => {
    listMemory.mockResolvedValue([entry({ slug: "other" })]);
    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(screen.getByTestId("memory-about-save")).not.toBeDisabled());
    fireEvent.change(textarea, { target: { value: "new about draft" } });

    // A search reloads the list and refreshes the still-missing about-me copy.
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "边界" } });
    await waitFor(() => expect(searchMemory).toHaveBeenCalled());
    expect(textarea.value).toBe("new about draft");

    fireEvent.click(screen.getByTestId("memory-about-save"));
    await waitFor(() =>
      expect(createMemory).toHaveBeenCalledWith(
        "ins_l",
        expect.objectContaining({ slug: "about-me", body: "new about draft" }),
      ),
    );
  });

  it("offers the conflict choice when the server moved under a dirty draft", async () => {
    const first = entry({ slug: "about-me", body: "theirs v1", revision: 1 });
    const moved = entry({ slug: "about-me", body: "theirs v2", revision: 2 });
    listMemory.mockResolvedValue([first]);
    let serverVersion = first;
    getMemory.mockImplementation(async () => serverVersion);

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(textarea.value).toBe("theirs v1"));
    fireEvent.change(textarea, { target: { value: "mine" } });
    serverVersion = moved;

    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "边界" } });
    await waitFor(() => expect(screen.getByTestId("memory-conflict")).toBeTruthy());
    expect(screen.getByTestId("memory-conflict-server")).toHaveTextContent("theirs v2");
    expect(screen.getByTestId("memory-conflict-mine")).toHaveTextContent("mine");
  });
});

describe("a plain session without a lineage row", () => {
  it("shows the no-lineage state and hides the editors", async () => {
    lineageExists.mockResolvedValue(false);
    listMemory.mockResolvedValue([]);

    renderPage("/s/ins_x/memory");

    await waitFor(() =>
      expect(screen.getByTestId("memory-empty")).toHaveTextContent("还不是连续血缘"),
    );
    expect(screen.queryByTestId("memory-about")).toBeNull();
    expect(screen.queryByTestId("memory-new")).toBeNull();
    expect(screen.queryByTestId("memory-search")).toBeNull();
    expect(createMemory).not.toHaveBeenCalled();
  });
});

describe("a delete whose revision went stale", () => {
  it("reloads and reopens the editor at the fresh revision", async () => {
    const row = entry({ slug: "pref", title: "Terse", revision: 1 });
    listMemory.mockResolvedValue([row]);
    getMemory.mockImplementation(async (_lineage, slug) => {
      if (slug === "pref") return entry({ slug: "pref", title: "Terse", revision: 2, body: "fresh" });
      throw new HubHttpError(404, "NOT_FOUND", "missing");
    });
    deleteMemory.mockRejectedValue(new HubHttpError(409, "MEMORY_REVISION_CONFLICT", "stale"));

    renderPage("/s/ins_x/memory");
    const rowButton = await screen.findByTestId("memory-row");
    fireEvent.click(rowButton);
    fireEvent.click(screen.getByTestId("memory-editor-delete"));
    fireEvent.click(screen.getByTestId("memory-delete-confirm-yes"));

    await waitFor(() =>
      expect(screen.getByTestId("memory-editor-error")).toHaveTextContent("已被更新"),
    );
    expect(within(screen.getByTestId("memory-editor")).getByText("rev 2")).toBeTruthy();
    // The list was reloaded so the row shows the fresh revision too.
    await waitFor(() => expect(listMemory.mock.calls.length).toBeGreaterThan(1));
  });
});

describe("about-me saves the draft's base revision", () => {
  it("answers a moved server with 409 again after the dialog was dismissed", async () => {
    const first = entry({ slug: "about-me", body: "theirs v1", revision: 1 });
    const moved = entry({ slug: "about-me", body: "theirs v2", revision: 2 });
    let serverVersion = first;
    listMemory.mockResolvedValue([first]);
    getMemory.mockImplementation(async () => serverVersion);
    updateMemory.mockRejectedValue(
      new HubHttpError(409, "MEMORY_REVISION_CONFLICT", "stale"),
    );

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(textarea.value).toBe("theirs v1"));
    fireEvent.change(textarea, { target: { value: "mine" } });

    // The other tab moves the entry; the conflict dialog opens.
    serverVersion = moved;
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "边界" } });
    await screen.findByTestId("memory-conflict");
    // Dismiss it (Escape) — the draft stays dirty, base stays rev 1.
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("memory-conflict")).toBeNull());

    fireEvent.click(screen.getByTestId("memory-about-save"));

    // The save must CAS on the DRAFT's base (rev 1), never the latest rev 2.
    await waitFor(() =>
      expect(updateMemory).toHaveBeenCalledWith("ins_l", "about-me", 1, { body: "mine" }),
    );
    // The 409 reopens the two-body choice rather than overwriting.
    expect(await screen.findByTestId("memory-conflict")).toBeTruthy();
    expect(screen.getByTestId("memory-conflict-server")).toHaveTextContent("theirs v2");
    expect(screen.getByTestId("memory-conflict-mine")).toHaveTextContent("mine");
  });
});

describe("the about-me editor waits for the server copy", () => {
  it("keeps the textarea disabled and the label neutral until the GET returns", async () => {
    let resolveAbout!: (value: MemoryEntry) => void;
    getMemory.mockImplementation(
      () =>
        new Promise<MemoryEntry>((resolve) => {
          resolveAbout = resolve;
        }) as never,
    );

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    expect(textarea).toBeDisabled();
    const card = screen.getByTestId("memory-about");
    expect(within(card).queryByText(/还没有；保存即创建/)).toBeNull();

    resolveAbout(entry({ slug: "about-me", body: "server body", revision: 1 }));
    await waitFor(() => expect(textarea).not.toBeDisabled());
    await waitFor(() => expect(textarea.value).toBe("server body"));
  });

  it("offers the conflict when a server copy arrives under a blind dirty draft", async () => {
    let server: MemoryEntry | null = null;
    getMemory.mockImplementation(async () => {
      if (!server) throw new HubHttpError(404, "NOT_FOUND", "missing");
      return server;
    });
    listMemory.mockResolvedValue([]);

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(screen.getByTestId("memory-about-save")).not.toBeDisabled());
    fireEvent.change(textarea, { target: { value: "blind draft" } });

    // Another tab creates the entry while this one only ever saw "missing".
    server = entry({ slug: "about-me", body: "server copy", revision: 1 });
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "边界" } });

    await screen.findByTestId("memory-conflict");
    expect(screen.getByTestId("memory-conflict-server")).toHaveTextContent("server copy");
    expect(screen.getByTestId("memory-conflict-mine")).toHaveTextContent("blind draft");
  });
});

describe("an archived server copy leaves a dirty draft alone", () => {
  it("keeps the typed draft instead of overwriting it with the archived body", async () => {
    const live = entry({ slug: "about-me", body: "v1", revision: 1 });
    let server: MemoryEntry = live;
    listMemory.mockResolvedValue([live]);
    getMemory.mockImplementation(async () => server);

    renderPage("/s/ins_x/memory");
    const textarea = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    await waitFor(() => expect(textarea.value).toBe("v1"));
    fireEvent.change(textarea, { target: { value: "mine" } });

    // The lineage retires while the draft is dirty.
    server = entry({
      slug: "about-me",
      body: "server v2",
      revision: 2,
      archivedAt: "2026-10-30T00:00:00.000Z",
    });
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "a" } });
    await waitFor(() => expect(screen.getByTestId("memory-about-archived")).toBeTruthy());

    // Restored elsewhere: the live card must still hold the owner's draft.
    server = entry({ slug: "about-me", body: "server v2", revision: 2 });
    fireEvent.change(await screen.findByTestId("memory-search"), { target: { value: "b" } });
    const restored = (await screen.findByTestId("memory-about-body")) as HTMLTextAreaElement;
    expect(restored.value).toBe("mine");
  });
});

describe("a delete target already removed elsewhere", () => {
  it("refreshes the list and closes the editor instead of reopening it stale", async () => {
    const row = entry({ slug: "gone", title: "Gone", revision: 1 });
    listMemory.mockResolvedValue([row]);
    deleteMemory.mockRejectedValue(new HubHttpError(404, "NOT_FOUND", "gone"));

    renderPage("/s/ins_x/memory");
    fireEvent.click(await screen.findByTestId("memory-row"));
    fireEvent.click(screen.getByTestId("memory-editor-delete"));
    fireEvent.click(screen.getByTestId("memory-delete-confirm-yes"));

    await waitFor(() => expect(screen.queryByTestId("memory-editor")).toBeNull());
    await waitFor(() => expect(listMemory.mock.calls.length).toBeGreaterThan(1));
  });
});
