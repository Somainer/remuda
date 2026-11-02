import { describe, expect, it } from "vitest";
import type { Instance } from "../../types/instance";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import { resumeCommand, rowActionKeys } from "./rowActions";

const T0 = "2026-10-31T08:00:00.000Z";

function inst(overrides: Partial<Instance> = {}): Instance {
  return {
    id: "ins_a" as Id,
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    driver: "claude-cli",
    lifecycle: "running",
    activity: known("working"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: { sessionId: unknownKnowledge("hub") } as Instance["nativeRef"],
    processRef: {} as Instance["processRef"],
    specRevision: "1",
    launchId: unknownKnowledge("none"),
    capabilities: {} as Instance["capabilities"],
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "jrn_1",
    durableSeq: "1",
    exit: na(),
    cwd: "/work/proj",
    ...overrides,
  } as Instance;
}

function interrupt(state: "supported" | "unsupported" | "unknown"): Instance["capabilities"] {
  return { capabilities: { interrupt: { state } } } as Instance["capabilities"];
}

describe("rowActionKeys", () => {
  it("offers 标为已看 only for a done-unseen row", () => {
    const done = inst({ activity: known("idle"), unseen: true });
    expect(rowActionKeys(done)).toContain("markSeen");
    expect(rowActionKeys(inst())).not.toContain("markSeen");
    expect(rowActionKeys(inst({ activity: known("idle") }))).not.toContain("markSeen");
  });

  it("offers 中断 only while a turn is running and interrupt is not unsupported", () => {
    expect(rowActionKeys(inst())).toContain("interrupt");
    expect(rowActionKeys(inst({ capabilities: interrupt("unknown") }))).toContain("interrupt");
    expect(rowActionKeys(inst({ capabilities: interrupt("unsupported") }))).not.toContain("interrupt");
    expect(rowActionKeys(inst({ activity: known("idle") }))).not.toContain("interrupt");
    expect(rowActionKeys(inst({ lifecycle: "exited", activity: known("idle") }))).not.toContain("interrupt");
  });

  it("always offers 重命名, and hides the copy and new items when their facts are missing", () => {
    const bare = inst({ cwd: null, workspaceId: "" as Id, hostId: "" as Id, nativeRef: { sessionId: unknownKnowledge("hub") } as Instance["nativeRef"] });
    const keys = rowActionKeys(bare);
    expect(keys).toContain("rename");
    expect(keys).not.toContain("copyCwd");
    expect(keys).not.toContain("copyResume");
    expect(keys).not.toContain("newInProject");
  });

  it("offers 复制恢复命令 only with a known native session id", () => {
    const withId = inst({ nativeRef: { sessionId: known("sess_abc") } as Instance["nativeRef"] });
    expect(rowActionKeys(withId)).toContain("copyResume");
    expect(resumeCommand(withId)).toBe("claude --resume sess_abc");
    const codex = inst({ kind: "codex", nativeRef: { sessionId: known("thr_1") } as Instance["nativeRef"] });
    expect(resumeCommand(codex)).toBe("codex --resume thr_1");
    expect(resumeCommand(inst())).toBeNull();
  });

  it("offers 关闭… only while the session is alive", () => {
    expect(rowActionKeys(inst())).toContain("close");
    expect(rowActionKeys(inst({ lifecycle: "exited" }))).not.toContain("close");
    expect(rowActionKeys(inst({ lifecycle: "failed" }))).not.toContain("close");
    expect(rowActionKeys(inst({ lifecycle: "closing" }))).not.toContain("close");
  });
});
