import { describe, expect, it } from "vitest";
import type { Host, Instance, UiStatus } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import type { InboxSource } from "./inboxRows";
import {
  contextRingLabel,
  deriveInboxRows,
  derivePushBanner,
  interactionHeadline,
  interactionRequestText,
  latestEventText,
  parseKindParam,
} from "./inboxRows";
import type { PushStatus } from "../../lib/push";

const T0 = "2026-09-20T10:00:00.000Z";
const T1 = "2026-09-20T11:00:00.000Z";
const T2 = "2026-09-20T12:00:00.000Z";

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
      description: "rm -rf /tmp/coord-media",
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

function source(overrides: Partial<InboxSource> = {}): InboxSource {
  return {
    interactions: [],
    instances: [],
    hosts: [host({ id: "hst_1" })],
    answering: {},
    phrases: {},
    rollups: {},
    deviceId: "dev_1",
    titleOf: (id) => `title ${id}`,
    hostName: (id) => `host ${id}`,
    workspaceLabel: (id) => `ws ${id}`,
    nowMs: Date.parse(T2),
    ...overrides,
  };
}

describe("deriveInboxRows tiering", () => {
  it("puts pending/answering/paused interactions into 待你处理 and nothing settled", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [
          interaction({ id: "int_pending", instanceId: "ins_pending", createdAt: T1 }),
          interaction({ id: "int_answering", instanceId: "ins_answering", createdAt: T0 }),
          interaction({
            id: "int_paused",
            instanceId: "ins_paused",
            hostId: "hst_off",
            createdAt: T2,
          }),
          interaction({
            id: "int_settled",
            instanceId: "ins_settled",
            state: "resolved",
            answer: {
              state: "known",
              value: {
                commandId: "cmd_1",
                actor: { principalId: "p", type: "human", deviceId: "dev_1", instanceId: null },
                value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:abab" },
                committedAt: T1,
              },
            },
          }),
          interaction({ id: "int_expired", instanceId: "ins_expired", state: "expired" }),
          interaction({ id: "int_invalid", instanceId: "ins_invalid", state: "invalidated" }),
        ],
        hosts: [host({ id: "hst_1" }), host({ id: "hst_off", state: "offline" })],
        answering: { int_answering: true },
      }),
    );
    expect(rows.pending.map((r) => r.interactionId)).toEqual([
      "int_paused",
      "int_pending",
      "int_answering",
    ]);
    expect(rows.pending.map((r) => r.uiState)).toEqual(["paused", "pending", "answering"]);
  });

  it("working/idle go to 进行中·最近, exited rows to 最近结束; starting/unknown/blocked nowhere", () => {
    const cases: Array<[UiStatus, Instance]> = [
      ["working", inst({ id: "ins_working", updatedAt: T2 })],
      ["idle", inst({ id: "ins_idle", lifecycle: "ready", activity: known("idle"), updatedAt: T1 })],
      [
        "exited",
        inst({
          id: "ins_exited",
          lifecycle: "exited",
          activity: known("idle"),
          updatedAt: T0,
          lastError: null,
        }),
      ],
      [
        "exited",
        inst({
          id: "ins_restart",
          lifecycle: "exited",
          activity: known("idle"),
          updatedAt: T0,
          lastError: "node-epoch-changed",
        }),
      ],
      ["starting", inst({ id: "ins_starting", lifecycle: "starting", activity: na() })],
      ["unknown", inst({ id: "ins_unknown", connectivity: "disconnected" })],
    ];
    const rows = deriveInboxRows(source({ instances: cases.map(([, i]) => i) }));
    expect(rows.recent.map((r) => r.instanceId)).toEqual(["ins_working", "ins_idle"]);
    expect(rows.recent.map((r) => r.status)).toEqual(["working", "idle"]);
    // c-endreason: ended rows never sit under a 进行中 heading.
    expect(rows.ended.map((r) => r.instanceId)).toEqual(["ins_restart", "ins_exited"]);
    expect(rows.ended.every((r) => r.status === "exited")).toBe(true);
  });

  it("never shows a blocked instance in both tiers", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_blocked" })],
        instances: [
          inst({
            id: "ins_blocked",
            activity: known("waiting-interaction"),
            updatedAt: T2,
          }),
        ],
      }),
    );
    expect(rows.pending.map((r) => r.instanceId)).toEqual(["ins_blocked"]);
    expect(rows.recent).toHaveLength(0);
  });

  it("a pending interaction hidden by a kind filter still blocks its instance from 进行中", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [
          interaction({
            id: "int_1",
            instanceId: "ins_blocked",
            kind: "question",
            request: { kind: "question", title: "AskUserQuestion", fields: [] },
          }),
        ],
        instances: [inst({ id: "ins_blocked", activity: known("waiting-interaction") })],
      }),
      { kind: "approval" },
    );
    expect(rows.pending).toHaveLength(0);
    expect(rows.recent).toHaveLength(0);
  });
});

describe("deriveInboxRows ordering", () => {
  it("sorts interactions newest first and recent instances by updatedAt", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [
          interaction({ id: "int_old", instanceId: "ins_a", createdAt: T0, updatedAt: T0 }),
          interaction({ id: "int_new", instanceId: "ins_b", createdAt: T2, updatedAt: T2 }),
          interaction({ id: "int_mid", instanceId: "ins_c", createdAt: T1, updatedAt: T1 }),
        ],
        instances: [
          inst({ id: "ins_recent_old", updatedAt: T0 }),
          inst({ id: "ins_recent_new", updatedAt: T2 }),
          inst({ id: "ins_recent_mid", updatedAt: T1 }),
        ],
      }),
    );
    expect(rows.pending.map((r) => r.interactionId)).toEqual(["int_new", "int_mid", "int_old"]);
    expect(rows.recent.map((r) => r.instanceId)).toEqual([
      "ins_recent_new",
      "ins_recent_mid",
      "ins_recent_old",
    ]);
  });
});

describe("subtitle: latest event text, errors first and verbatim", () => {
  it("prints instance.lastError verbatim even when a live phrase exists", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_1" })],
        instances: [inst({ id: "ins_1", lastError: "API Error: Request rejected (429)" })],
        phrases: { ins_1: "Workflow wf_9 · phase compile" },
      }),
    );
    expect(rows.pending[0].subtitle).toBe("API Error: Request rejected (429)");
  });

  it("prints the interaction request verbatim even when a journal phrase exists", () => {
    // The raised request is the actionable event on a blocked turn; an older
    // journal line must never bury the text the decision is about.
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_1" })],
        instances: [inst({ id: "ins_1" })],
        phrases: { ins_1: "echo: m inbox allow once" },
      }),
    );
    expect(rows.pending[0].subtitle).toBe("rm -rf /tmp/coord-media");
  });

  it("uses the verbatim live phrase on instance rows with no interaction", () => {
    const rows = deriveInboxRows(
      source({
        instances: [
          inst({
            id: "ins_1",
            lifecycle: "ready",
            activity: known("idle"),
          }),
        ],
        phrases: { ins_1: "Bash ninja -C build" },
      }),
    );
    expect(rows.recent[0].subtitle).toBe("Bash ninja -C build");
  });

  it("falls back to the request text verbatim, never an invented status", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_1" })],
        instances: [inst({ id: "ins_1" })],
      }),
    );
    expect(rows.pending[0].subtitle).toBe("rm -rf /tmp/coord-media");
  });

  it("ended rows land in 最近结束 with the human sentence; the raw code hides in detail", () => {
    const restart = deriveInboxRows(
      source({
        instances: [
          inst({
            id: "ins_1",
            lifecycle: "exited",
            activity: known("idle"),
            lastError: "node-epoch-changed",
          }),
        ],
      }),
    );
    expect(restart.recent).toHaveLength(0);
    const row = restart.ended[0]!;
    expect(row.subtitle).toBe("Node 重启，会话已中断");
    expect(row.end).toEqual({
      label: "Node 重启，会话已中断",
      detail: "node-epoch-changed",
      tone: "interrupted",
    });
  });

  it("an unknown failed code is a neutral 已结束 row, never red", () => {
    const rows = deriveInboxRows(
      source({
        instances: [
          inst({
            id: "ins_1",
            lifecycle: "failed",
            activity: known("idle"),
            lastError: "turn failed: upstream timeout",
          }),
        ],
      }),
    );
    const row = rows.ended[0]!;
    expect(row.subtitle).toBe("已结束");
    expect(row.end?.tone).toBe("ended");
    expect(row.end?.detail).toBe("turn failed: upstream timeout");
  });

  it("caps 最近结束 at MAX_ENDED_ROWS, newest first", () => {
    const instances = Array.from({ length: 12 }, (_, i) =>
      inst({
        id: `ins_${String(i).padStart(2, "0")}`,
        lifecycle: "exited",
        activity: known("idle"),
        updatedAt: `2026-09-20T${String(10 + Math.floor(i / 2)).padStart(2, "0")}:00:00.000Z`,
      }),
    );
    const rows = deriveInboxRows(source({ instances }));
    expect(rows.ended).toHaveLength(10);
    expect(rows.ended[0]!.instanceId).toBe("ins_11");
  });

  it("exposes latestEventText with null when nothing is known", () => {
    expect(latestEventText(inst({ id: "ins_1" }), undefined)).toBeNull();
    expect(latestEventText(inst({ id: "ins_1" }), "   ")).toBeNull();
  });

  it("keeps native-tty question lines verbatim and joined", () => {
    const item = interaction({
      id: "int_q",
      instanceId: "ins_1",
      kind: "question",
      carrier: "native-tty",
      request: {
        kind: "question",
        title: "终端提问",
        fields: [
          { id: "q0", title: "继续吗", description: "继续吗？(y/n)", input: "text", required: true, options: [], allowFreeText: true, sensitive: false },
        ],
      },
    });
    expect(interactionHeadline(item)).toBe("终端提问");
    expect(interactionRequestText(item)).toBe("继续吗？(y/n)");
  });
});

describe("paused projection", () => {
  it("marks rows paused when the host is offline", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_1", hostId: "hst_off" })],
        instances: [inst({ id: "ins_1", hostId: "hst_off" })],
        hosts: [host({ id: "hst_off", state: "offline" })],
      }),
    );
    expect(rows.pending[0].uiState).toBe("paused");
    expect(rows.pending[0].options).toHaveLength(2);
  });

  it("marks rows paused on a disconnected instance even with an online host", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_1" })],
        instances: [inst({ id: "ins_1", connectivity: "disconnected" })],
      }),
    );
    expect(rows.pending[0].uiState).toBe("paused");
  });
});

describe("?focus= and ?kind=", () => {
  it("flags only the focused interaction row", () => {
    const withFocus = deriveInboxRows(
      source({
        interactions: [
          interaction({ id: "int_1", instanceId: "ins_1" }),
          interaction({ id: "int_2", instanceId: "ins_2" }),
        ],
        instances: [inst({ id: "ins_1" }), inst({ id: "ins_2" })],
      }),
      { focus: "int_2" },
    );
    expect(withFocus.pending.find((r) => r.interactionId === "int_2")?.focused).toBe(true);
    expect(withFocus.pending.find((r) => r.interactionId === "int_1")?.focused).toBe(false);
  });

  it("filters the interaction tier exactly like /approvals", () => {
    const question = interaction({
      id: "int_q",
      instanceId: "ins_q",
      kind: "question",
      request: { kind: "question", title: "AskUserQuestion", fields: [] },
    });
    const approval = interaction({ id: "int_a", instanceId: "ins_a" });
    const src = source({ interactions: [question, approval] });
    expect(
      deriveInboxRows(src, { kind: "question" }).pending.map((r) => r.interactionId),
    ).toEqual(["int_q"]);
    expect(
      deriveInboxRows(src, { kind: "approval" }).pending.map((r) => r.interactionId),
    ).toEqual(["int_a"]);
    expect(parseKindParam("question")).toBe("question");
    expect(parseKindParam("bogus")).toBe("all");
    expect(parseKindParam(null)).toBe("all");
  });

  it("question rows go to the session (goAnswer); approvals carry one-tap options", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [
          interaction({
            id: "int_q",
            instanceId: "ins_q",
            kind: "question",
            request: { kind: "question", title: "AskUserQuestion", fields: [] },
          }),
          interaction({ id: "int_a", instanceId: "ins_a" }),
        ],
      }),
    );
    const q = rows.pending.find((r) => r.interactionId === "int_q")!;
    const a = rows.pending.find((r) => r.interactionId === "int_a")!;
    expect(q.goAnswer).toBe(true);
    expect(q.options).toEqual([]);
    expect(a.goAnswer).toBe(false);
    expect(a.options.map((o) => o.label)).toEqual(["允许一次", "拒绝"]);
  });
});

describe("context ring pct", () => {
  it("renders neither ring nor number when contextPct is null, and the pct when known", () => {
    const rows = deriveInboxRows(
      source({
        interactions: [interaction({ id: "int_1", instanceId: "ins_known" })],
        instances: [
          inst({ id: "ins_known", usageRollup: { contextPct: 78 } as Instance["usageRollup"] }),
          inst({ id: "ins_unknown" }),
        ],
        rollups: { ins_unknown: { contextPct: null } as never },
      }),
    );
    expect(rows.pending[0].contextPct).toBe(78);
    expect(rows.recent.find((r) => r.instanceId === "ins_unknown")?.contextPct).toBeNull();
  });

  it("exposes the accessibility readout the ring's aria-label uses, null when no ring renders", () => {
    expect(contextRingLabel(null)).toBeNull();
    expect(contextRingLabel(78)).toBe("上下文剩余 78%");
    expect(contextRingLabel(0)).toBe("上下文剩余 0%");
    expect(contextRingLabel(142)).toBe("上下文剩余 100%");
  });

  it("bakes contextPct into the memo sig in both tiers (stale-ring guard)", () => {
    const base = source({
      interactions: [interaction({ id: "int_1", instanceId: "ins_blocked" })],
      instances: [
        inst({ id: "ins_blocked" }),
        inst({ id: "ins_recent" }),
      ],
      rollups: {},
    });
    const before = deriveInboxRows(base);
    const after = deriveInboxRows({
      ...base,
      rollups: { ins_blocked: { contextPct: 40 }, ins_recent: { contextPct: 61 } } as never,
    });
    expect(after.pending[0]!.sig).not.toBe(before.pending[0]!.sig);
    expect(after.recent.find((r) => r.instanceId === "ins_recent")!.sig).not.toBe(
      before.recent.find((r) => r.instanceId === "ins_recent")!.sig,
    );
  });
});

describe("derivePushBanner", () => {
  const status = (patch: Partial<PushStatus>): PushStatus => ({
    permission: "default",
    subscribed: false,
    endpoint: null,
    needsHomeScreen: false,
    ...patch,
  });

  it("hides before the status has been read and when permission is granted", () => {
    expect(derivePushBanner(null)).toEqual({ show: false });
    expect(derivePushBanner(status({ permission: "granted", subscribed: false }))).toEqual({
      show: false,
    });
    expect(derivePushBanner(status({ permission: "granted", subscribed: true }))).toEqual({
      show: false,
    });
  });

  it("offers 开启 for default and denied permissions", () => {
    expect(derivePushBanner(status({ permission: "default" }))).toEqual({
      show: true,
      mode: "enable",
    });
    expect(derivePushBanner(status({ permission: "denied" }))).toEqual({
      show: true,
      mode: "enable",
    });
  });

  it("stays hidden when notifications are unsupported off iOS", () => {
    expect(derivePushBanner(status({ permission: "unsupported" }))).toEqual({ show: false });
  });

  it("switches to the home-screen action on iOS when not standalone", () => {
    expect(
      derivePushBanner(status({ permission: "unsupported", needsHomeScreen: true })),
    ).toEqual({ show: true, mode: "homescreen" });
    // Granted on a home-screen PWA still hides.
    expect(
      derivePushBanner(status({ permission: "granted", needsHomeScreen: true })),
    ).toEqual({ show: false });
  });
});
