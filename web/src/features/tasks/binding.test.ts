import { beforeEach, describe, expect, it } from "vitest";
import {
  buildTaskCreateBody,
  dispatchDirectory,
  forgetBindingChoice,
  hasRememberedBinding,
  isOutOfTree,
  isQueuedSharing,
  parseBinding,
  rememberedBindingChoice,
  rememberBindingChoice,
  sharedWithTasksLabel,
} from "./binding";

const HOST = "hst_e2e";
const WSP = "wsp_e2e";

describe("parseBinding", () => {
  it("reuse root carries no worktree name (dir key is the registered root)", () => {
    const parsed = parseBinding({
      mode: "reuse",
      hostId: HOST,
      workspaceId: WSP,
    });
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) throw new Error("expected ok");
    expect(parsed.binding.mode).toBe("reuse");
    expect(parsed.binding.worktreeName).toBeUndefined();
  });

  it("reuse can target an existing remuda-wt sibling by name", () => {
    const parsed = parseBinding({
      mode: "reuse",
      hostId: HOST,
      workspaceId: WSP,
      worktreeName: "agent-one",
    });
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) throw new Error("expected ok");
    expect(parsed.binding.worktreeName).toBe("agent-one");
    expect(parsed.binding).not.toHaveProperty("base");
  });

  it("pool requires a pool name and carries its base override", () => {
    const missing = parseBinding({ mode: "pool", hostId: HOST, workspaceId: WSP });
    expect(missing).toEqual({ ok: false, error: "pool-name-required" });

    const parsed = parseBinding({
      mode: "pool",
      hostId: HOST,
      workspaceId: WSP,
      worktreeName: "alpha",
      base: "develop",
    });
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) throw new Error("expected ok");
    expect(parsed.binding.mode).toBe("pool");
    expect(parsed.binding.worktreeName).toBe("alpha");
    expect(parsed.binding.base).toBe("develop");
  });

  it("rejects out-of-tree targets exactly as resolve_instance_cwd does", () => {
    const outOfTree = [
      "../escape",
      "/abs/path",
      "a/b",
      ".hidden",
      "-dash",
      "remuda-wt/agent",
      "foo..bar/..",
      "name with space",
    ];
    for (const name of outOfTree) {
      expect(isOutOfTree(name)).toBe(true);
      const reuse = parseBinding({
        mode: "reuse",
        hostId: HOST,
        workspaceId: WSP,
        worktreeName: name,
      });
      expect(reuse).toEqual({ ok: false, error: "out-of-tree" });
    }
  });

  it("rejects the Node-reserved pool slot suffix", () => {
    const parsed = parseBinding({
      mode: "pool",
      hostId: HOST,
      workspaceId: WSP,
      worktreeName: "alpha-s1",
    });
    expect(parsed).toEqual({ ok: false, error: "reserved-slot-suffix" });
    // The suffix rule is anchored: "alpha-skip" is an ordinary pool name.
    const ordinary = parseBinding({
      mode: "pool",
      hostId: HOST,
      workspaceId: WSP,
      worktreeName: "alpha-skip",
    });
    expect(ordinary.ok).toBe(true);
  });

  it("requires host and workspace ids", () => {
    expect(parseBinding({ mode: "reuse", hostId: "", workspaceId: WSP })).toEqual({
      ok: false,
      error: "host-required",
    });
    expect(parseBinding({ mode: "reuse", hostId: HOST, workspaceId: " " })).toEqual({
      ok: false,
      error: "workspace-required",
    });
  });
});

describe("dispatchDirectory folds the binding into the existing wire fields", () => {
  it("reuse root leaves cwd unset so the Node resolves the registered root", () => {
    const { binding } = expectOk(
      parseBinding({ mode: "reuse", hostId: HOST, workspaceId: WSP }),
    );
    expect(dispatchDirectory(binding, undefined)).toEqual({
      kind: "root",
      cwd: undefined,
      worktree: undefined,
    });
  });

  it("reuse sibling flows only through cwd (no worktree field)", () => {
    const { binding } = expectOk(
      parseBinding({
        mode: "reuse",
        hostId: HOST,
        workspaceId: WSP,
        worktreeName: "agent-one",
      }),
    );
    const dir = dispatchDirectory(binding, "/tmp/repo/remuda-wt/agent-one");
    expect(dir).toEqual({
      kind: "reuse",
      cwd: "/tmp/repo/remuda-wt/agent-one",
      worktree: undefined,
    });
  });

  it("pool flows through the existing worktree field with the leased slot", () => {
    const { binding } = expectOk(
      parseBinding({
        mode: "pool",
        hostId: HOST,
        workspaceId: WSP,
        worktreeName: "alpha",
      }),
    );
    // The Hub rewrites the stored binding to the Node-assigned slot name
    // before dispatch; the parser-side choice names only the pool.
    const leased = { ...binding, worktreeName: "alpha-s2" };
    const dir = dispatchDirectory(leased, "/tmp/repo/remuda-wt/alpha-s2");
    expect(dir).toEqual({
      kind: "pool",
      cwd: "/tmp/repo/remuda-wt/alpha-s2",
      worktree: "alpha-s2",
    });
  });

  it("never silently switches: a missing catalog path refuses", () => {
    const { binding } = expectOk(
      parseBinding({
        mode: "reuse",
        hostId: HOST,
        workspaceId: WSP,
        worktreeName: "gone",
      }),
    );
    expect(() => dispatchDirectory(binding, undefined)).toThrow(/no catalog path/);
  });
});

describe("shared directory projection", () => {
  it("surfaces 与 N 个 task 共用 from refcount and flags queued sharing", () => {
    expect(sharedWithTasksLabel(1)).toBeNull();
    expect(sharedWithTasksLabel(2)).toBe("与 2 个 task 共用");
    expect(sharedWithTasksLabel(3)).toBe("与 3 个 task 共用");
    expect(isQueuedSharing({ refcount: 1, queued: false })).toBe(false);
    expect(isQueuedSharing({ refcount: 2, queued: true })).toBe(true);
    // The store row is the authority even if the Node omitted queued.
    expect(isQueuedSharing({ refcount: 2, queued: false })).toBe(true);
  });
});

describe("per-project memory (D2 force-choose-then-remember)", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("starts with no choice, remembers the last one per project", () => {
    expect(hasRememberedBinding("prj_a")).toBe(false);
    expect(rememberedBindingChoice("prj_a")).toBeNull();

    rememberBindingChoice("prj_a", { mode: "reuse" });
    rememberBindingChoice("prj_b", { mode: "pool", worktreeName: "alpha" });
    expect(rememberedBindingChoice("prj_a")).toEqual({ mode: "reuse" });
    expect(rememberedBindingChoice("prj_b")).toEqual({
      mode: "pool",
      worktreeName: "alpha",
    });

    rememberBindingChoice("prj_a", { mode: "pool", worktreeName: "beta" });
    expect(rememberedBindingChoice("prj_a")).toEqual({
      mode: "pool",
      worktreeName: "beta",
    });

    forgetBindingChoice("prj_a");
    expect(hasRememberedBinding("prj_a")).toBe(false);
    // Another project's memory is untouched.
    expect(rememberedBindingChoice("prj_b")?.mode).toBe("pool");
  });

  it("ignores corrupt storage instead of defaulting silently", () => {
    localStorage.setItem("runtime.task-binding.prj_x", "{not json");
    expect(rememberedBindingChoice("prj_x")).toBeNull();
    expect(hasRememberedBinding("prj_x")).toBe(false);
  });
});

describe("buildTaskCreateBody", () => {
  it("omits the binding entirely when unbound", () => {
    const body = buildTaskCreateBody({
      projectId: "prj_a",
      title: "t",
      intent: "i",
    });
    expect(body.workspaceBinding).toBeUndefined();
  });

  it("attaches a parsed binding and rejects structurally invalid choices", () => {
    const body = buildTaskCreateBody({
      projectId: "prj_a",
      title: "t",
      intent: "i",
      binding: { mode: "pool", hostId: HOST, workspaceId: WSP, worktreeName: "alpha" },
    });
    expect(body.workspaceBinding).toMatchObject({
      mode: "pool",
      hostId: HOST,
      workspaceId: WSP,
      worktreeName: "alpha",
    });
    expect(() =>
      buildTaskCreateBody({
        projectId: "prj_a",
        title: "t",
        intent: "i",
        binding: { mode: "reuse", hostId: HOST, workspaceId: WSP, worktreeName: "../x" },
      }),
    ).toThrow(/out-of-tree/);
  });
});

function expectOk(parsed: ReturnType<typeof parseBinding>) {
  if (!parsed.ok) throw new Error(`expected parsed binding, got ${parsed.error}`);
  return parsed;
}
