import { describe, expect, it } from "vitest";
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import { deriveApprovalRows, type ApprovalFilters, type ApprovalSource } from "./approvalRows";
import { deriveInboxQueue } from "../mobile/inboxRows";
import {
  CARRIER_LABEL,
  DEPARTED_STATUS_TEXT,
  QUEUE_STATUS_TEXT,
  carrierLabel,
  deadlineLabel,
  decisionPreview,
  decisionTitle,
  statusDotOf,
} from "./approvalRows";

const T0 = "2026-09-20T10:00:00.000Z";
const T1 = "2026-09-20T11:00:00.000Z";

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

function approval(overrides: Partial<Interaction> & { id: Id; instanceId: Id }): Interaction {
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
      description: `rm -rf /tmp/${overrides.id}`,
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

const FILTERS: ApprovalFilters = { kind: "all", hostId: "", workspaceId: "", focus: null };

function answeredOn(deviceId: string | null): Interaction["answer"] {
  return deviceId == null
    ? na()
    : known({
        commandId: "cmd_1",
        actor: { principalId: "prin_1", type: "human", deviceId, instanceId: null },
        value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:abab" },
        committedAt: T1,
      });
}

function source(interactions: Interaction[], instances: Instance[] = []): ApprovalSource {
  return {
    interactions,
    instances:
      instances.length > 0
        ? instances
        : [inst({ id: "ins_1" })],
    hosts: [host({ id: "hst_1" })],
    answering: {},
    deviceId: "dev_1",
    workspaceLabel: (id) => `ws ${id}`,
  };
}

describe("deriveApprovalRows tiering", () => {
  it("queue tier is exactly the shared deriveInboxQueue membership, unfiltered", () => {
    // c-ghostbadge round 2: no second QUEUE_STATES loop — desktop and the
    // compact inbox/badge must derive the same queue set.
    const pending = approval({ id: "itx_p", instanceId: "ins_1" });
    const answering = approval({ id: "itx_a", instanceId: "ins_1" });
    const paused = approval({ id: "itx_z", instanceId: "ins_off", hostId: "hst_off" });
    const expired = approval({ id: "itx_e", instanceId: "ins_1", state: "expired" });
    const superseded = approval({ id: "itx_i", instanceId: "ins_1", state: "invalidated" });
    const settled = approval({
      id: "itx_s",
      instanceId: "ins_1",
      state: "answer-committed",
      answer: {
        state: "known",
        value: {
          commandId: "cmd_1",
          actor: { principalId: "p", type: "human", deviceId: "dev_1", instanceId: null },
          value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:abab" },
          committedAt: T1,
        },
      },
    });
    const s = source(
      [pending, answering, paused, expired, superseded, settled],
      [inst({ id: "ins_1" }), inst({ id: "ins_off", hostId: "hst_off" })],
    );
    s.hosts = [host({ id: "hst_1" }), host({ id: "hst_off", state: "offline" })];
    s.answering = { itx_a: true };

    const { queue, departed } = deriveApprovalRows(s, FILTERS);
    const sharedIds = deriveInboxQueue(s).map((q) => q.item.id).sort();
    expect(queue.map((r) => r.item.id).sort()).toEqual(sharedIds);
    expect(sharedIds).toEqual(["itx_a", "itx_p", "itx_z"]);
    expect(departed.map((r) => r.item.id).sort()).toEqual(["itx_e", "itx_i"]);
  });

  it("queues a pending approval on a connected online host", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(1);
    expect(departed).toHaveLength(0);
    expect(queue[0]!.uiState).toBe("pending");
    expect(queue[0]!.item).toBe(item);
    expect(queue[0]!.instance?.id).toBe("ins_1");
    expect(queue[0]!.focused).toBe(false);
  });

  it("marks an interaction POSTed locally as answering", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const { queue } = deriveApprovalRows(
      { ...source([item]), answering: { itx_1: true } },
      FILTERS,
    );
    expect(queue[0]!.uiState).toBe("answering");
  });

  it("pauses the queue row when the host is offline", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const s = source([item]);
    s.hosts = [host({ id: "hst_1", state: "offline" })];
    const { queue } = deriveApprovalRows(s, FILTERS);
    expect(queue[0]!.uiState).toBe("paused");
  });

  it("sends expired and invalidated interactions to departed", () => {
    const expired = approval({ id: "itx_e", instanceId: "ins_1", state: "expired" });
    const superseded = approval({ id: "itx_i", instanceId: "ins_1", state: "invalidated" });
    const { queue, departed } = deriveApprovalRows(source([expired, superseded]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed.map((row) => row.uiState)).toEqual(["expired", "superseded"]);
  });

  it("a passed deadline moves a still-pending interaction to departed as expired", () => {
    const item = approval({
      id: "itx_1",
      instanceId: "ins_1",
      deadline: known("2026-09-20T09:00:00.000Z"),
    });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed[0]!.uiState).toBe("expired");
  });

  it("hides an answer committed on THIS device everywhere", () => {
    const item = approval({
      id: "itx_1",
      instanceId: "ins_1",
      state: "answer-committed",
      answer: answeredOn("dev_1"),
    });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed).toHaveLength(0);
  });

  it("keeps a committed answer from another device in departed as superseded", () => {
    const item = approval({
      id: "itx_1",
      instanceId: "ins_1",
      state: "resolved",
      answer: answeredOn("dev_other"),
    });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed[0]!.uiState).toBe("superseded");
  });

  it("treats a committed answer with no recorded actor as settled", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1", state: "resolved" });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed).toHaveLength(0);
  });

  it("keeps a resolved interaction in departed when its deadline has passed", () => {
    // projectInteraction checks expiry before the committed branch; the
    // settled prefilter must mirror that order or this row would vanish.
    const item = approval({
      id: "itx_1",
      instanceId: "ins_1",
      state: "resolved",
      answer: answeredOn("dev_1"),
      deadline: known("2026-09-20T09:00:00.000Z"),
    });
    const { queue, departed } = deriveApprovalRows(source([item]), FILTERS);
    expect(queue).toHaveLength(0);
    expect(departed[0]!.uiState).toBe("expired");
  });
});

describe("deriveApprovalRows filters", () => {
  it("filters by interaction kind before joining", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const { queue } = deriveApprovalRows(source([item]), { ...FILTERS, kind: "question" });
    expect(queue).toHaveLength(0);
  });

  it("filters by host", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1", hostId: "hst_other" });
    const { queue, departed } = deriveApprovalRows(source([item]), {
      ...FILTERS,
      hostId: "hst_1",
    });
    expect(queue).toHaveLength(0);
    expect(departed).toHaveLength(0);
  });

  it("filters by workspace via the joined instance", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const { queue } = deriveApprovalRows(source([item]), {
      ...FILTERS,
      workspaceId: "wsp_other",
    });
    expect(queue).toHaveLength(0);
  });

  it("marks the focused row and reflects focus in its sig", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const unfocused = deriveApprovalRows(source([item]), FILTERS).queue[0]!;
    const focused = deriveApprovalRows(source([item]), { ...FILTERS, focus: "itx_1" })
      .queue[0]!;
    expect(unfocused.focused).toBe(false);
    expect(focused.focused).toBe(true);
    expect(focused.sig).not.toBe(unfocused.sig);
  });
});

describe("deriveApprovalRows sig (poll re-parse memoization)", () => {
  it("stays equal across fresh deep-equal parses with new object identity", () => {
    const first = approval({ id: "itx_1", instanceId: "ins_1" });
    const second = JSON.parse(JSON.stringify(first)) as Interaction;
    const a = deriveApprovalRows(source([first]), FILTERS).queue[0]!;
    const b = deriveApprovalRows(source([second]), FILTERS).queue[0]!;
    expect(a.sig).toBe(b.sig);
  });

  it("changes when state, answerability or request content changes", () => {
    const before = deriveApprovalRows(
      source([approval({ id: "itx_1", instanceId: "ins_1" })]),
      FILTERS,
    ).queue[0]!;
    const after = deriveApprovalRows(
      source([
        approval({
          id: "itx_1",
          instanceId: "ins_1",
          request: {
            kind: "approval",
            title: "Bash",
            description: "rm -rf /tmp/other",
            toolCallId: null,
            actionRef: "act_1",
            options: [
              { id: "allow-once", label: "允许一次", effect: "allow-once", nativeValueRef: "act_1" },
              { id: "deny", label: "拒绝", effect: "deny", nativeValueRef: "act_1" },
            ],
            requestedPermissionsRef: null,
            inputDigest: "sha256:cdcd",
          },
        }),
      ]),
      FILTERS,
    ).queue[0]!;
    expect(after.sig).not.toBe(before.sig);
  });

  it("changes when the instance connectivity or host state changes", () => {
    const item = approval({ id: "itx_1", instanceId: "ins_1" });
    const online = deriveApprovalRows(source([item]), FILTERS).queue[0]!;

    const s = source([item]);
    s.hosts = [host({ id: "hst_1", state: "offline" })];
    const offline = deriveApprovalRows(s, FILTERS).queue[0]!;
    expect(offline.sig).not.toBe(online.sig);
    expect(offline.uiState).toBe("paused");
  });
});

describe("deriveApprovalRows joins and order", () => {
  it("joins each interaction to its own instance via the index (O(1) Map)", () => {
    const items = [
      approval({ id: "itx_1", instanceId: "ins_1" }),
      approval({ id: "itx_2", instanceId: "ins_2" }),
    ];
    const instances = [
      inst({ id: "ins_1", workspaceId: "wsp_a" }),
      inst({ id: "ins_2", workspaceId: "wsp_b" }),
    ];
    const { queue } = deriveApprovalRows(source(items, instances), FILTERS);
    expect(queue.map((row) => row.instance?.workspaceId)).toEqual(["wsp_a", "wsp_b"]);
  });

  it("preserves interaction array order within each tier", () => {
    const items = [
      approval({ id: "itx_1", instanceId: "ins_1" }),
      approval({ id: "itx_2", instanceId: "ins_1" }),
      approval({ id: "itx_3", instanceId: "ins_1" }),
    ];
    const { queue } = deriveApprovalRows(source(items), FILTERS);
    expect(queue.map((row) => row.item.id)).toEqual(["itx_1", "itx_2", "itx_3"]);
  });
});

describe("decision card presentational helpers", () => {
  it("labels every known carrier and maps unsupported to 未知", () => {
    expect(Object.keys(CARRIER_LABEL).sort()).toEqual(
      [
        "acp-rpc",
        "claude-control",
        "claude-hook",
        "codex-rpc",
        "harness-hook",
        "native-tty",
        "unsupported",
      ].sort(),
    );
    expect(carrierLabel("native-tty")).toBe("终端屏幕");
    expect(carrierLabel("harness-hook")).toBe("工具钩子");
    expect(carrierLabel("unsupported")).toBe("未知");
    for (const label of Object.values(CARRIER_LABEL)) expect(label.length).toBeGreaterThan(0);
  });

  it("renders an unknown/absent deadline as an em dash, never 0 or 无限期", () => {
    const unknownDeadline = approval({ id: "itx_1", instanceId: "ins_1" });
    expect(unknownDeadline.deadline.state).not.toBe("known");
    expect(deadlineLabel(unknownDeadline)).toBe("—");
  });

  it("renders a known deadline as a clock time rather than a dash", () => {
    const knownDeadline = approval({
      id: "itx_1",
      instanceId: "ins_1",
      deadline: known("2026-09-20T09:05:00.000Z"),
    });
    const label = deadlineLabel(knownDeadline);
    expect(label).not.toBe("—");
    expect(label).toMatch(/\d{1,2}[:：]\d{2}/);
  });

  it("encodes the §2.5 status sentences", () => {
    expect(QUEUE_STATUS_TEXT.pending).toBe("等待你的选择");
    expect(QUEUE_STATUS_TEXT.answering).toBe("已提交 · 等待确认");
    expect(QUEUE_STATUS_TEXT.paused).toBe("主机离线，交互暂停");
    expect(DEPARTED_STATUS_TEXT.expired).toContain("过期");
    expect(DEPARTED_STATUS_TEXT.superseded).toContain("其它设备");
  });

  it("maps each ui state to a shape-encoded status dot", () => {
    expect(statusDotOf("pending")).toBe("blocked");
    expect(statusDotOf("answering")).toBe("working");
    expect(statusDotOf("paused")).toBe("unknown");
    expect(statusDotOf("expired")).toBe("exited");
    expect(statusDotOf("superseded")).toBe("idle");
  });

  it("titles approvals by tool name and terminal questions by carrier", () => {
    expect(decisionTitle(approval({ id: "i", instanceId: "ins_1" }))).toBe("Bash");
    const tty = approval({
      id: "i",
      instanceId: "ins_1",
      kind: "question",
      carrier: "native-tty",
      request: { kind: "question", title: "x", fields: [] },
    });
    expect(decisionTitle(tty)).toBe("终端提问");
  });

  it("previews approvals verbatim and composes the hook-question line", () => {
    expect(decisionPreview(approval({ id: "i", instanceId: "ins_1" }))).toBe(
      "rm -rf /tmp/i",
    );
    const field = {
      id: "q0",
      title: "下一步",
      description: null,
      input: "single-select" as const,
      required: true,
      options: [],
      allowFreeText: false,
      sensitive: false,
    };
    const hookQuestion = approval({
      id: "i",
      instanceId: "ins_1",
      kind: "question",
      carrier: "claude-control",
      request: { kind: "question", title: "AskUserQuestion", fields: [field, { ...field, id: "q1" }] },
    });
    expect(decisionPreview(hookQuestion)).toBe("问你 2 题 · AskUserQuestion");
  });
});
