import { describe, expect, it } from "vitest";
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Task } from "../../types/generated";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import {
  GLANCE_COUNT_MAX,
  TOP_LEVEL_WORKING,
  formatGlanceCount,
  formatUpdatedAt,
  glanceCounts,
  glancePieces,
  glanceText,
  greetingForHour,
  routedToMainCount,
  routedToMainLine,
  type GlanceSource,
} from "./glance";

/**
 * home.md §5.1/§5.2 count rules and the §5.2 segment/greeting grammar. Pure
 * derivations only — the surfaces' geometry lives in the hub spec.
 */

const T0 = "2026-10-31T08:00:00.000Z";
const T1 = "2026-10-31T09:00:00.000Z";
const T2 = "2026-10-31T10:00:00.000Z";
const NOW = Date.parse(T2);

function inst(overrides: Partial<Instance> & { id: Id }): Instance {
  return {
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
    nativeRef: {} as Instance["nativeRef"],
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
    ...overrides,
  } as Instance;
}

function host(overrides: Partial<Host> & { id: Id }): Host {
  return {
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    label: `host ${overrides.id}`,
    ownerPrincipalId: "prin_1",
    state: "online",
    transport: { mode: "outbound-wss", endpointRef: "ep_1" },
    ...overrides,
  } as Host;
}

function interaction(overrides: Partial<Interaction> & { id: Id; instanceId: Id }): Interaction {
  return {
    revision: "1",
    createdAt: T1,
    updatedAt: T1,
    runId: null,
    hostId: "hst_1",
    kind: "approval",
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "tool" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "hst_1",
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: "Bash",
      description: "rm -rf build",
      toolCallId: null,
      actionRef: "act_1",
      options: [
        { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: "act_1" },
        { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "act_1" },
      ],
      requestedPermissionsRef: null,
      inputDigest: "sha256:abab",
    },
    deadline: unknownKnowledge("none"),
    deadlineSource: "none",
    answer: na(),
    delivery: "not-sent",
    resolution: na(),
    ...overrides,
  } as Interaction;
}

function task(overrides: Partial<Task> & { id: string }): Task {
  return {
    projectId: "prj_1",
    title: "task",
    class: "implement",
    state: "running",
    budget: {},
    mandate: {} as Task["mandate"],
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    blockedReason: null,
    ...overrides,
  } as Task;
}

function source(overrides: Partial<GlanceSource> = {}): GlanceSource { return sourceOf(overrides); }

function sourceOf(overrides: Partial<GlanceSource> = {}): GlanceSource {
  return {
    instances: [],
    interactions: [],
    hosts: [host({ id: "hst_1" })],
    tasks: [],
    answering: {},
    deviceId: "dev_1",
    nowMs: NOW,
    ...overrides,
  };
}

describe("routedTo (D-061): N counts only what waits on the owner", () => {
  const routed = () =>
    source({
      instances: [inst({ id: "ins_w", taskId: "tsk_w" as Id }), inst({ id: "ins_o" })],
      interactions: [
        interaction({ id: "int_owner", instanceId: "ins_o" }),
        interaction({ id: "int_main", instanceId: "ins_w", routedTo: "main" }),
        interaction({ id: "int_main2", instanceId: "ins_w", routedTo: "main" }),
        interaction({ id: "int_agent", instanceId: "ins_o", routedTo: "agent" }),
      ],
      tasks: [task({ id: "tsk_w", blockedReason: "等 Main" })],
    });

  it("A drops routed items, and a routed item does not cover its task (B)", () => {
    // A = int_owner; B = tsk_w, whose only interactions went to Main — the
    // queue shows its 任务待你 card, exactly as the Hub badge counts it.
    expect(glanceCounts(routed()).needYou).toBe(2);
  });

  it("the Main card counts live items routed to Main; unknown before the first page", () => {
    const base = { ...routed(), hydrated: true };
    expect(routedToMainCount(base)).toBe(2);
    expect(routedToMainCount({ ...base, hydrated: false })).toBeNull();
    // A settled item is no longer being handled.
    expect(
      routedToMainCount({
        ...base,
        interactions: base.interactions.map((row) =>
          row.id === "int_main2" ? { ...row, state: "resolved" as const } : row,
        ),
      }),
    ).toBe(1);
    expect(routedToMainLine(1)).toBe("正在处理 1 个 worker 提问");
    expect(routedToMainLine(120)).toBe("正在处理 99+ 个 worker 提问");
  });
});

describe("glanceCounts: §5.1 N = A + B", () => {
  it("counts the interactions really waiting on the owner and drops settled, expired and superseded rows", () => {
    const counts = glanceCounts(
      source({
        instances: [inst({ id: "ins_a" })],
        interactions: [
          interaction({ id: "int_a", instanceId: "ins_a" }),
          // Deadline in the past: the inbox projection calls it 已离队.
          interaction({ id: "int_b", instanceId: "ins_a", deadline: known(T0) }),
          interaction({ id: "int_c", instanceId: "ins_a", state: "resolved" }),
          interaction({ id: "int_d", instanceId: "ins_a", state: "invalidated" }),
        ],
      }),
    );
    expect(counts.needYou).toBe(1);
  });

  it("counts a paused (host-offline) interaction: it still waits on the owner", () => {
    const counts = glanceCounts(
      source({
        instances: [inst({ id: "ins_a", hostId: "hst_off" })],
        hosts: [host({ id: "hst_off", state: "offline" })],
        interactions: [interaction({ id: "int_a", instanceId: "ins_a", hostId: "hst_off" })],
      }),
    );
    expect(counts.needYou).toBe(1);
  });

  it("adds an owner-blocked task only when none of its sessions is waiting on an interaction (§4.6)", () => {
    const counts = glanceCounts(
      source({
        instances: [
          inst({ id: "ins_taskblocked", taskId: "tsk_1" as Id }),
          inst({ id: "ins_withint", taskId: "tsk_2" as Id }),
          inst({ id: "ins_quiet", taskId: "tsk_3" as Id }),
          inst({ id: "ins_archived", taskId: "tsk_4" as Id }),
        ],
        interactions: [interaction({ id: "int_a", instanceId: "ins_withint" })],
        tasks: [
          task({ id: "tsk_1", blockedReason: "等你确认验收范围" }),
          task({ id: "tsk_2", blockedReason: "等你" }),
          task({ id: "tsk_3", blockedReason: "   " }),
          task({ id: "tsk_4", blockedReason: "等你", archivedAt: T1 }),
        ],
      }),
    );
    // A = 1 interaction, B = tsk_1 only.
    expect(counts.needYou).toBe(2);
  });

  it("counts a blocked task whose only interaction already expired (it is no longer the card shown)", () => {
    const counts = glanceCounts(
      source({
        instances: [inst({ id: "ins_a", taskId: "tsk_1" as Id })],
        interactions: [interaction({ id: "int_a", instanceId: "ins_a", deadline: known(T0) })],
        tasks: [task({ id: "tsk_1", blockedReason: "等你" })],
      }),
    );
    expect(counts.needYou).toBe(1);
  });

  it("counts Main's question once: it is one interaction, never a second queue card", () => {
    const counts = glanceCounts(
      source({
        instances: [inst({ id: "ins_main", grants: ["address-owner"] })],
        interactions: [interaction({ id: "int_main", instanceId: "ins_main" })],
      }),
    );
    expect(counts.needYou).toBe(1);
  });

  it("counts 工作中 from the §3.1 rank, not from connectivity", () => {
    const counts = glanceCounts(
      source({
        instances: [
          inst({ id: "ins_work" }),
          // draining is rank 3 too.
          inst({ id: "ins_drain", activity: known("draining") }),
          // live with an unknown activity is rank 3 (h).
          inst({ id: "ins_unknown", activity: na() }),
          // A dropped connection does not change the rank.
          inst({ id: "ins_offline_host", connectivity: "disconnected" }),
          inst({ id: "ins_idle", activity: known("idle") }),
          inst({ id: "ins_blocked", activity: known("waiting-interaction") }),
          inst({ id: "ins_exited", lifecycle: "exited", activity: known("idle") }),
          inst({ id: "ins_starting", lifecycle: "starting", activity: na() }),
        ],
      }),
    );
    expect(counts.working).toBe(4);
  });

  it("counts 工作中 from the instance set the surface actually lists", () => {
    // Desktop /sessions drops child sessions (its rows and caption say so);
    // /m's ledger includes them. One function, two sets.
    const source = sourceOf({
      instances: [
        inst({ id: "ins_parent" }),
        inst({ id: "ins_child", parent: { instanceId: "ins_parent", runId: "run_1", commandId: "cmd_1" } }),
      ],
    });
    expect(glanceCounts(source).working).toBe(2);
    expect(
      glanceCounts(source, source.instances.filter(TOP_LEVEL_WORKING)).working,
    ).toBe(1);
  });

  it("never counts the same instance twice", () => {
    const counts = glanceCounts(
      source({ instances: [inst({ id: "ins_a" }), inst({ id: "ins_a" })] }),
    );
    expect(counts.working).toBe(1);
  });

  it("counts ◉ from the done-unseen rank: idle, unseen, and not a failed turn", () => {
    const counts = glanceCounts(
      source({
        instances: [
          inst({ id: "ins_done", activity: known("idle"), unseen: true }),
          // A failed turn is a queue card (§4.5), never ◉.
          inst({ id: "ins_failed", activity: known("idle"), unseen: true, lastTurnError: { at: T1, text: "boom" } }),
          // Seen, or still working: not ◉.
          inst({ id: "ins_seen", activity: known("idle"), unseen: false }),
          inst({ id: "ins_working", activity: known("working"), unseen: true }),
          // §3.1 matches done-unseen BEFORE awaiting-wake, so an unseen parent
          // whose child is still running stays ◉.
          inst({ id: "ins_waiting", activity: known("idle"), unseen: true }),
          inst({ id: "ins_child", activity: known("working"), parent: { instanceId: "ins_waiting", runId: "run_1", commandId: "cmd_1" } }),
        ],
      }),
    );
    expect(counts.doneUnseen).toBe(2);
  });

  it("counts only Main's current chapter: an older or fenced chapter of the Main lineage is not ◉", () => {
    const counts = glanceCounts(
      source({
        instances: [
          // The live current chapter is ◉.
          inst({ id: "ins_current", activity: known("idle"), unseen: true, grants: ["address-owner"], lineageId: "lin_main" as Id, generation: 2 }),
          // The fenced predecessor shares the lineage and is not shown.
          inst({ id: "ins_old", activity: known("idle"), unseen: true, grants: ["address-owner"], lineageId: "lin_main" as Id, generation: 1, fencedAt: T1 }),
          // An ordinary session on another lineage still counts.
          inst({ id: "ins_other", activity: known("idle"), unseen: true }),
        ],
      }),
    );
    expect(counts.doneUnseen).toBe(2);
  });

  it("counts ◉ from the instance set the surface lists, like 工作中", () => {
    const source = sourceOf({
      instances: [
        inst({ id: "ins_parent", activity: known("idle"), unseen: true }),
        inst({ id: "ins_child", activity: known("idle"), unseen: true, parent: { instanceId: "ins_parent", runId: "run_1", commandId: "cmd_1" } }),
      ],
    });
    expect(glanceCounts(source).doneUnseen).toBe(2);
    expect(glanceCounts(source, source.instances.filter(TOP_LEVEL_WORKING)).doneUnseen).toBe(1);
  });
});

describe("§5.2 numbers", () => {
  it("spells 99 out and writes 99+ above it", () => {
    expect(GLANCE_COUNT_MAX).toBe(99);
    expect(formatGlanceCount(0)).toBe("0");
    expect(formatGlanceCount(99)).toBe("99");
    expect(formatGlanceCount(100)).toBe("99+");
    expect(formatGlanceCount(1234)).toBe("99+");
  });

  it("greets by the local hour in the three §5.2 bands", () => {
    expect(greetingForHour(4)).toBe("晚上好");
    expect(greetingForHour(5)).toBe("早上好");
    expect(greetingForHour(11)).toBe("早上好");
    expect(greetingForHour(12)).toBe("下午好");
    expect(greetingForHour(17)).toBe("下午好");
    expect(greetingForHour(18)).toBe("晚上好");
    expect(greetingForHour(23)).toBe("晚上好");
    expect(greetingForHour(0)).toBe("晚上好");
  });

  it("formats the stale row's 更新于 clock", () => {
    const ms = new Date(2026, 9, 31, 9, 5).getTime();
    expect(formatUpdatedAt(ms)).toBe("09:05");
  });
});

describe("glancePieces: §5.2 段的取舍", () => {
  it("reads 「–」 for every segment before the first snapshot, greeting or not", () => {
    const pieces = glancePieces({ counts: null, hour: 9, hostsOnline: 3, hostsTotal: 4 });
    expect(pieces).toEqual([
      { key: "needYou", count: null },
      { key: "doneUnseen", count: null },
      { key: "working", count: null },
    ]);
    expect(glanceText(pieces)).toBe("⚠– 需要你 · ◉– 完成未看 · ●– 工作中");
  });

  it("hides a segment known to be 0", () => {
    expect(glanceText(glancePieces({ counts: { needYou: 2, working: 0 }, hour: 9, hostsOnline: 1, hostsTotal: 1 })))
      .toBe("⚠2 需要你");
    const greeting = glancePieces({ counts: { needYou: 0, working: 5 }, hour: 15, hostsOnline: 1, hostsTotal: 1 });
    expect(glanceText(greeting)).toBe("下午好 · 都处理完了 · ●5 工作中");
  });

  it("writes 99+ on a large segment", () => {
    expect(glanceText(glancePieces({ counts: { needYou: 120, working: 150 }, hour: 9, hostsOnline: 1, hostsTotal: 1 })))
      .toBe("⚠99+ 需要你 · ●99+ 工作中");
  });

  it("ends an empty queue with the non-zero segments, else with N/M 主机在线", () => {
    expect(
      glanceText(glancePieces({ counts: { needYou: 0, working: 2 }, hour: 7, hostsOnline: 4, hostsTotal: 4 })),
    ).toBe("早上好 · 都处理完了 · ●2 工作中");
    expect(
      glanceText(glancePieces({ counts: { needYou: 0, working: 0 }, hour: 20, hostsOnline: 3, hostsTotal: 4 })),
    ).toBe("晚上好 · 都处理完了 · 3/4 主机在线");
  });

  it("keeps the 工作中 segment alive while its filter is active, even at zero", () => {
    expect(
      glanceText(
        glancePieces({ counts: { needYou: 0, working: 0 }, hour: 9, hostsOnline: 3, hostsTotal: 4, workingFilter: true }),
      ),
    ).toBe("早上好 · 都处理完了 · ●0 工作中");
    // Without the filter the known-zero segment stays hidden (§5.2).
    expect(
      glanceText(glancePieces({ counts: { needYou: 0, working: 0 }, hour: 9, hostsOnline: 3, hostsTotal: 4 })),
    ).toBe("早上好 · 都处理完了 · 3/4 主机在线");
  });

  it("drops the host tail when no host is registered at all", () => {
    expect(glanceText(glancePieces({ counts: { needYou: 0, working: 0 }, hour: 9, hostsOnline: 0, hostsTotal: 0 })))
      .toBe("早上好 · 都处理完了");
  });

  it("places ◉ 完成未看 between 需要你 and 工作中, hides it at 0, writes 99+, and reads 「–」 before the snapshot", () => {
    expect(
      glanceText(glancePieces({ counts: { needYou: 2, doneUnseen: 4, working: 1 }, hour: 9, hostsOnline: 1, hostsTotal: 1 })),
    ).toBe("⚠2 需要你 · ◉4 完成未看 · ●1 工作中");
    // A known zero is hidden, like the other segments (§5.2).
    expect(
      glanceText(glancePieces({ counts: { needYou: 2, doneUnseen: 0, working: 0 }, hour: 9, hostsOnline: 1, hostsTotal: 1 })),
    ).toBe("⚠2 需要你");
    expect(
      glanceText(glancePieces({ counts: { needYou: 0, doneUnseen: 150, working: 0 }, hour: 9, hostsOnline: 1, hostsTotal: 1 })),
    ).toBe("早上好 · 都处理完了 · ◉99+ 完成未看");
    // Before the first snapshot every segment, including ◉, reads 「–」.
    const pieces = glancePieces({ counts: null, hour: 9, hostsOnline: 1, hostsTotal: 1 });
    expect(pieces).toEqual([
      { key: "needYou", count: null },
      { key: "doneUnseen", count: null },
      { key: "working", count: null },
    ]);
    expect(glanceText(pieces)).toBe("⚠– 需要你 · ◉– 完成未看 · ●– 工作中");
  });

  it("keeps the ◉ segment alive at zero while its filter is active", () => {
    expect(
      glanceText(
        glancePieces({ counts: { needYou: 0, doneUnseen: 0, working: 0 }, hour: 9, hostsOnline: 3, hostsTotal: 4, doneUnseenFilter: true }),
      ),
    ).toBe("早上好 · 都处理完了 · ◉0 完成未看 · 3/4 主机在线");
  });
});
