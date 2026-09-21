import { describe, expect, it } from "vitest";
import type { ScmEntry, ScmStatus } from "../../types/scm";
import type { FilesViewModel } from "../files/filesViewModel";
import {
  entryInTaskSpace,
  entryPaths,
  filterTaskEntries,
  globMatch,
  normalizeGlob,
  normalizeGlobs,
  taskHasSession,
  taskScopeFrom,
  taskSpaceEntries,
  type TaskSpaceScope,
} from "./taskSpaceFilter";

// Mirrors the Rust table in
// crates/remuda-protocol/src/task.rs glob_matching_covers_stars_and_recursive_stars,
// because the Hub gate matches owns with the same rules.
describe("globMatch / normalizeGlob — parity with remuda_protocol", () => {
  it("matches literal paths exactly", () => {
    expect(globMatch("crates/remuda-hub/src/tasks.rs", "crates/remuda-hub/src/tasks.rs")).toBe(true);
    expect(globMatch("crates/remuda-hub/src/tasks.rs", "crates/other.rs")).toBe(false);
  });

  it("keeps * inside one path segment", () => {
    expect(globMatch("*.rs", "tasks.rs")).toBe(true);
    expect(globMatch("*.rs", "src/tasks.rs")).toBe(false);
    expect(globMatch("crates/*/src/lib.rs", "crates/remuda-hub/src/lib.rs")).toBe(true);
    expect(globMatch("crates/*/src/lib.rs", "crates/a/b/src/lib.rs")).toBe(false);
  });

  it("lets ** cross directory boundaries", () => {
    expect(globMatch("crates/**", "crates/remuda-hub/src/tasks.rs")).toBe(true);
    expect(globMatch("crates/**", "crates/x.rs")).toBe(true);
    expect(globMatch("**/tasks.rs", "crates/remuda-hub/src/tasks.rs")).toBe(true);
    expect(globMatch("crates/**/tasks.rs", "crates/tasks.rs")).toBe(true);
    expect(globMatch("crates/**/tasks.rs", "crates/a/b/tasks.rs")).toBe(true);
  });

  it("makes ? match exactly one non-slash byte", () => {
    expect(globMatch("a?.rs", "a1.rs")).toBe(true);
    expect(globMatch("a?.rs", "a/b.rs")).toBe(false);
  });

  it("normalises claims like normalize_glob", () => {
    expect(normalizeGlob("foo/")).toBe("foo/**");
    expect(normalizeGlob("./crates/x.rs")).toBe("crates/x.rs");
    expect(normalizeGlob("  crates/x ")).toBe("crates/x");
    expect(normalizeGlob("\\crates\\x.rs")).toBe("crates/x.rs");
    expect(normalizeGlob("/crates/x.rs")).toBe("crates/x.rs");
    expect(normalizeGlob("")).toBe("**");
    expect(normalizeGlob("/")).toBe("**");
    expect(normalizeGlob("a/./b")).toBe("a/./b"); // only a leading ./ is stripped
  });

  it("sorts and de-duplicates the glob set", () => {
    expect(normalizeGlobs(["b/", "a/**", "b/**", "b/"])).toEqual(["a/**", "b/**"]);
  });
});

const entry = (path: string, extra: Partial<ScmEntry> = {}): ScmEntry => ({
  path,
  origPath: null,
  xy: " M",
  kind: "modified",
  sizeBytes: 10,
  ...extra,
});

const scope = (owns: readonly string[], sessionIds: readonly string[] = []): TaskSpaceScope => ({
  taskId: "tsk_1",
  sessionIds,
  owns,
});

describe("filterTaskEntries — the task-space projection", () => {
  const rows = [
    entry("src/main.rs"),
    entry("notes/todo.md", { xy: "??", kind: "untracked" }),
    entry("assets/logo.bin"),
    entry("crates/remuda-hub/src/tasks.rs"),
  ];

  it("keeps only rows covered by an owns glob and never mutates the input", () => {
    const scoped = filterTaskEntries(rows, scope(["src/**", "notes/**"]));
    expect(scoped.map((row) => row.path)).toEqual(["src/main.rs", "notes/todo.md"]);
    expect(rows).toHaveLength(4);
  });

  it("supports recursive and suffix globs", () => {
    expect(filterTaskEntries(rows, scope(["**/tasks.rs"])).map((row) => row.path)).toEqual([
      "crates/remuda-hub/src/tasks.rs",
    ]);
    expect(filterTaskEntries(rows, scope(["*.bin"]))).toEqual([]);
    expect(filterTaskEntries(rows, scope(["assets/*"])).map((row) => row.path)).toEqual([
      "assets/logo.bin",
    ]);
  });

  it("keeps a rename when either the new or the old path is owned", () => {
    const rename = entry("src/new.rs", { origPath: "src/old.rs", xy: "R ", kind: "renamed" });
    expect(entryInTaskSpace(rename, scope(["src/old.rs"]))).toBe(true);
    expect(entryInTaskSpace(rename, scope(["src/new.rs"]))).toBe(true);
    expect(entryInTaskSpace(rename, scope(["docs/**"]))).toBe(false);
    expect(entryPaths(rename)).toEqual(["src/new.rs", "src/old.rs"]);
  });

  it("projects to an empty list when nothing is inside owns (no synthesised rows)", () => {
    expect(filterTaskEntries(rows, scope(["docs/**"]))).toEqual([]);
  });

  it("projects to an empty list when the task declared no owns at all", () => {
    // The gate treats an empty owns as an empty scope; an unattributed
    // worktree-wide change must not be claimed for the task.
    expect(filterTaskEntries(rows, scope([]))).toEqual([]);
  });

  it("projects an empty status to an empty list", () => {
    expect(filterTaskEntries([], scope(["src/**"]))).toEqual([]);
  });

  it("normalises directory-style owns before matching", () => {
    expect(filterTaskEntries(rows, scope(["src/"])).map((row) => row.path)).toEqual([
      "src/main.rs",
    ]);
    expect(filterTaskEntries(rows, scope(["./notes/"])).map((row) => row.path)).toEqual([
      "notes/todo.md",
    ]);
  });
});

function view(phase: FilesViewModel["phase"], entries: ScmEntry[] = []): FilesViewModel {
  const status: ScmStatus | null =
    phase === "changes" || phase === "clean"
      ? {
          workspaceId: "wsp_x",
          root: "/tmp/repo",
          scm: "git",
          availability: "ok",
          observedAt: "2026-09-21T00:00:00.000Z",
          entries,
          limits: { maxEntries: 5000, maxDiffBytes: 262144, maxFileBytes: 1048576 },
          truncated: { entries: false, entriesOmitted: 0, nonUtf8Omitted: 0, statusBytes: false },
        }
      : null;
  return { phase, status, contentChanged: false };
}

describe("taskSpaceEntries — §3.6 availability pass-through", () => {
  const rows = [entry("src/main.rs"), entry("assets/logo.bin")];

  it("projects a resolved changes view, possibly to an empty task space", () => {
    expect(taskSpaceEntries(view("changes", rows), scope(["src/**"]))?.map((r) => r.path)).toEqual([
      "src/main.rs",
    ]);
    expect(taskSpaceEntries(view("changes", rows), scope(["docs/**"]))).toEqual([]);
  });

  it.each([
    "loading",
    "not-collected",
    "clean",
    "unsupported",
    "forbidden",
    "offline",
    "missing",
    "failed",
  ] as const)("passes the %s phase through untouched (never fakes a baseline)", (phase) => {
    expect(taskSpaceEntries(view(phase), scope(["src/**"]))).toBeNull();
  });
});

describe("taskScopeFrom — session set from the placement ledger", () => {
  const task = { id: "tsk_1", owns: ["src/**"] } as never;

  it("collects distinct instance ids in ledger order", () => {
    const built = taskScopeFrom(task, [
      { instanceId: "ins_a" },
      { instanceId: "ins_b" },
      { instanceId: "ins_a" },
      { instanceId: null },
      {},
    ]);
    expect(built.taskId).toBe("tsk_1");
    expect(built.sessionIds).toEqual(["ins_a", "ins_b"]);
    expect(built.owns).toEqual(["src/**"]);
    expect(taskHasSession(built, "ins_a")).toBe(true);
    expect(taskHasSession(built, "ins_z")).toBe(false);
  });

  it("defaults an absent owns and an empty ledger", () => {
    const built = taskScopeFrom({ id: "tsk_2" } as never);
    expect(built.owns).toEqual([]);
    expect(built.sessionIds).toEqual([]);
  });
});
