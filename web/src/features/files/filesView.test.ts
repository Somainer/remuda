import { describe, expect, it } from "vitest";
import { HubHttpError } from "../../lib/httpError";
import {
  diffIsStale,
  fileIsStale,
  formatBytes,
  formatCollectedAt,
  isUntracked,
  kindLabel,
  phaseFromError,
  projectDiff,
  projectFile,
  projectStatus,
  type ScmDiff,
  type ScmEntry,
  type ScmFile,
  type ScmStatus,
} from "./filesViewModel";

const limits = { maxEntries: 5000, maxDiffBytes: 262144, maxFileBytes: 1048576 };

function status(overrides: Partial<ScmStatus> = {}): ScmStatus {
  return {
    workspaceId: "wsp_x",
    root: "/tmp/repo",
    scm: "git",
    availability: "ok",
    headOid: "aaaa",
    branch: { state: "known", value: "main" },
    observedAt: "2026-09-14T12:00:00.000Z",
    entries: [],
    limits,
    truncated: { entries: false, entriesOmitted: 0, nonUtf8Omitted: 0, statusBytes: false },
    ...overrides,
  };
}

const modified: ScmEntry = {
  path: "src/a.rs",
  xy: " M",
  kind: "modified",
  sizeBytes: 12,
  oldOid: "1",
  newOid: "2",
};
const untracked: ScmEntry = { path: "u.txt", xy: "??", kind: "untracked", sizeBytes: 3 };

describe("projectStatus — §3.6 stays distinct, never merged into 加载失败", () => {
  it("empty entry set is the clean state", () => {
    expect(projectStatus(status()).phase).toBe("clean");
  });

  it("non-empty entry set is the changes state", () => {
    expect(projectStatus(status({ entries: [modified] })).phase).toBe("changes");
  });

  it("unsupported availability is its own phase with the reason", () => {
    const model = projectStatus(
      status({ availability: "unsupported", unsupportedReason: "not-a-git-repository" }),
    );
    expect(model.phase).toBe("unsupported");
    expect(model.reason).toBe("not-a-git-repository");
  });

  it("denied availability maps to the forbidden phase", () => {
    const model = projectStatus(
      status({ availability: "denied", deniedReason: "timed-out" }),
    );
    expect(model.phase).toBe("forbidden");
    expect(model.reason).toBe("timed-out");
  });
});

describe("phaseFromError — 409/422 offline, never a generic failure", () => {
  it("HOST_OFFLINE (409) is offline", () => {
    const error = new HubHttpError(409, "HOST_OFFLINE", "offline");
    expect(phaseFromError(error).phase).toBe("offline");
  });

  it("PLACEMENT_UNSATISFIABLE (422) is offline", () => {
    const error = new HubHttpError(422, "PLACEMENT_UNSATISFIABLE", "x");
    expect(phaseFromError(error).phase).toBe("offline");
  });

  it("404 is missing workspace", () => {
    expect(phaseFromError(new HubHttpError(404, "NOT_FOUND", "x")).phase).toBe("missing");
  });

  it("403 is forbidden", () => {
    expect(phaseFromError(new HubHttpError(403, "FORBIDDEN", "x")).phase).toBe("forbidden");
  });

  it("other failures stay retriable rather than impersonating a state", () => {
    expect(phaseFromError(new HubHttpError(500, "INTERNAL", "boom")).phase).toBe("failed");
  });
});

describe("entry detail — binary, truncation, and §3.7 staleness", () => {
  const diff = (overrides: Partial<ScmDiff> = {}): ScmDiff => ({
    availability: "ok",
    headOid: "aaaa",
    staged: false,
    observedAt: "2026-09-14T12:00:00.000Z",
    items: [{ path: "src/a.rs", patch: "+x", binary: false, truncated: false, bytesAvailable: 2 }],
    truncated: { diffBytes: false, bytesOmitted: 0 },
    ...overrides,
  });

  it("renders a textual diff", () => {
    const state = projectDiff(modified, status(), diff());
    expect(state.kind).toBe("diff");
  });

  it("marks a binary diff entry-level unsupported (whole view stays available)", () => {
    const state = projectDiff(
      modified,
      status(),
      diff({ items: [{ path: "src/a.rs", patch: null, binary: true, truncated: false, bytesAvailable: 0 }] }),
    );
    expect(state.kind).toBe("binary");
  });

  it("flags a diff taken against a moved HEAD as changed", () => {
    const state = projectDiff(modified, status(), diff({ headOid: "bbbb" }));
    expect(state.kind).toBe("diff");
    if (state.kind === "diff") expect(state.changed).toBe(true);
    expect(diffIsStale(status(), diff({ headOid: "bbbb" }))).toBe(true);
    expect(diffIsStale(status(), diff({ headOid: "aaaa" }))).toBe(false);
  });

  const file = (overrides: Partial<ScmFile> = {}): ScmFile => ({
    availability: "ok",
    path: "u.txt",
    headOid: "aaaa",
    observedAt: "2026-09-14T12:00:00.000Z",
    mediaType: "text/plain",
    binary: false,
    sizeBytes: 3,
    digest: { state: "known", value: "sha256:abc" },
    content: "abc",
    truncated: false,
    ...overrides,
  });

  it("previews an untracked text file", () => {
    const state = projectFile(untracked, status(), file());
    expect(state.kind).toBe("preview");
  });

  it("does not inline a binary file", () => {
    const state = projectFile(
      untracked,
      status(),
      file({ binary: true, mediaType: "application/octet-stream", content: null }),
    );
    expect(state.kind).toBe("binary");
  });

  it("marks an over-limit file too-large without bytes", () => {
    const state = projectFile(
      { ...untracked, sizeBytes: 2_000_000 },
      status(),
      file({ truncated: true, sizeBytes: 2_000_000, content: null }),
    );
    expect(state.kind).toBe("too-large");
  });

  it("detects content change by size and by head", () => {
    expect(fileIsStale(untracked, status(), file({ sizeBytes: 9 }))).toBe(true);
    expect(fileIsStale(untracked, status(), file({ headOid: "bbbb" }))).toBe(true);
    expect(fileIsStale(untracked, status(), file())).toBe(false);
  });

  it("treats only ?? as untracked", () => {
    expect(isUntracked(untracked)).toBe(true);
    expect(isUntracked(modified)).toBe(false);
  });
});

describe("labels and formatting", () => {
  it("maps every XY/kind to a Chinese label", () => {
    expect(kindLabel({ ...untracked })).toBe("未跟踪");
    expect(kindLabel({ ...modified })).toBe("修改");
    expect(kindLabel({ path: "a", xy: "A ", kind: "added" })).toBe("新增");
    expect(kindLabel({ path: "a", xy: "D ", kind: "deleted" })).toBe("删除");
    expect(kindLabel({ path: "a", xy: "R ", kind: "renamed" })).toBe("重命名");
    expect(kindLabel({ path: "a", xy: "UU", kind: "unmerged" })).toBe("冲突");
  });

  it("formats byte sizes", () => {
    expect(formatBytes(null)).toBe("—");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
  });

  it("formats the collected-at timestamp", () => {
    expect(formatCollectedAt("2026-09-14T12:00:00.000Z")).toMatch(/2026/);
    expect(formatCollectedAt(undefined)).toBe("—");
  });
});
