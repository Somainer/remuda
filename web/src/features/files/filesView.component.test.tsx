import { act, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

// Ordered-load race coverage for FilesView. The ux-files:175 gate flake is a
// response-ordering race between two concurrent fetchChanges calls (the two
// dev StrictMode mount loads; a double refresh is the same code path here):
// the OLDER request's response can land AFTER the newer one and after the
// reader opened a stale-HEAD detail, replacing the view model and silently
// clearing contentChanged — 手动刷新 disappears for the whole visit. The
// pure projection logic stays in filesView.test.ts.
const mocks = vi.hoisted(() => ({
  changes: vi.fn(),
  diff: vi.fn(),
  file: vi.fn(),
}));
vi.mock("./filesApi", () => ({
  fetchChanges: mocks.changes,
  fetchEntryDiff: mocks.diff,
  fetchEntryFile: mocks.file,
}));

import { FilesView } from "./FilesView";

function statusResponse() {
  return {
    workspaceId: "wsp_g2_changed",
    root: "/tmp/remuda-g2/changed",
    scm: "git",
    availability: "ok",
    headOid: "9c2f1a4b7d8e0f11223344556677889900aabbcc",
    branch: { state: "known", value: "feat/workbench-g2" },
    observedAt: "2026-09-14T12:00:00.000Z",
    entries: [
      {
        path: "src/edited.rs",
        origPath: null,
        xy: " M",
        kind: "modified",
        sizeBytes: 64,
        oldOid: "3333333333333333333333333333333333333333",
        newOid: "4444444444444444444444444444444444444444",
        digest: { state: "unknown", reason: "not-collected" },
      },
    ],
    limits: { maxEntries: 5000, maxDiffBytes: 262144, maxFileBytes: 1048576 },
    truncated: { entries: false, entriesOmitted: 0, nonUtf8Omitted: 0, statusBytes: false },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function renderView() {
  return render(
    <MemoryRouter>
      <FilesView hostId="hst_e2e" workspaceId="wsp_g2_changed" onBack={() => {}} />
    </MemoryRouter>,
  );
}

describe("FilesView ordered loads", () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it("drops an older load response that lands after the newer view and an open changed detail", async () => {
    // load 1 = mount; load 2 = an immediate second load (StrictMode replay in
    // dev, a quick refresh here). Both are in flight; the newer one resolves
    // first; the stale one resolves only after the stale-HEAD detail opened.
    const first = deferred<ReturnType<typeof statusResponse>>();
    const second = deferred<ReturnType<typeof statusResponse>>();
    mocks.changes
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    mocks.diff.mockResolvedValue({
      workspaceId: "wsp_g2_changed",
      scm: "git",
      availability: "ok",
      headOid: "ffffffffffffffffffffffffffffffffffffffff",
      staged: false,
      observedAt: "2026-09-14T12:00:05.000Z",
      items: [{ path: "src/edited.rs", patch: "+changed\n", binary: false, truncated: false, bytesAvailable: 9 }],
      truncated: { diffBytes: false, bytesOmitted: 0 },
    });

    renderView();
    expect(mocks.changes).toHaveBeenCalledTimes(1);
    // The concurrent second load (what the StrictMode replay effects).
    fireEvent.click(screen.getByTestId("files-refresh"));
    expect(mocks.changes).toHaveBeenCalledTimes(2);

    // Newer load lands first; the entry opens against a moved HEAD.
    await act(async () => {
      second.resolve(statusResponse());
    });
    const entry = await screen.findByText(/src\/edited\.rs/);
    fireEvent.click(entry);
    await screen.findByTestId("files-content-changed");
    expect(screen.getByTestId("files-content-refresh")).toBeTruthy();

    // The stale first response arrives now. Pre-fix it replaced the view and
    // cleared contentChanged + the open detail; the guard must drop it.
    await act(async () => {
      first.resolve(statusResponse());
    });
    expect(screen.getByTestId("files-content-changed")).toBeTruthy();
    expect(screen.getByTestId("files-content-refresh")).toBeTruthy();
    expect(screen.getByTestId("files-detail")).toBeTruthy();
  });
});
