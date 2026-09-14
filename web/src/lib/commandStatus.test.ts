import { describe, expect, it } from "vitest";
import type { Command } from "../types/command";
import type { Connectivity, Host, Lifecycle } from "../types/instance";
import type { Interaction } from "../types/interaction";
import type { Capability, CapabilityProvision, CapabilitySnapshot } from "../types/nativeRef";
import { known, unknownKnowledge, type Id } from "../types/wire";
import { printCapabilities } from "./capabilities";
import {
  acceptedLabel,
  canSubmitAnswer,
  COMMAND_STATUS_LABEL,
  NODE_EPOCH_CHANGED,
  projectCommandStatus,
  projectDeletion,
  type CommandStatusFacts,
} from "./commandStatus";

function command(
  state: Command["state"],
  dispatch: Command["dispatch"],
  resolution: Command["resolution"] = "clear",
): CommandStatusFacts["command"] {
  return { state, dispatch, resolution };
}

function instance(patch: Partial<{ lifecycle: Lifecycle; connectivity: Connectivity; lastError: string | null }> = {}) {
  return { lifecycle: "running" as Lifecycle, connectivity: "connected" as Connectivity, ...patch };
}

function host(state: Host["state"] = "online"): Host {
  return {
    id: "hst" as Id,
    revision: "1",
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
    label: "box",
    ownerPrincipalId: "prn" as Id,
    state,
    transport: { mode: "outbound-wss", endpointRef: "obj" as Id },
  };
}

function interaction(patch: Partial<Interaction> = {}): Interaction {
  return {
    id: "int" as Id,
    revision: "1",
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
    instanceId: "ins" as Id,
    runId: null,
    hostId: "hst" as Id,
    kind: "approval",
    requestKey: { native: { type: "none" }, processGeneration: "1", runGeneration: null, connectionEpoch: "epoch" as Id },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: "Bash",
      description: "ls",
      toolCallId: null,
      actionRef: "obj" as Id,
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:00",
    },
    deadline: unknownKnowledge("none"),
    deadlineSource: "none",
    answer: { state: "not-applicable" },
    delivery: "not-sent",
    resolution: { state: "not-applicable" },
    ...patch,
  };
}

/** An answer committed on this device. */
function answeredBy(deviceId: string): Interaction["answer"] {
  return known({
    commandId: "cmd" as Id,
    actor: { principalId: "prn" as Id, type: "human", deviceId, instanceId: null },
    value: { kind: "approval", optionId: "allow-once", inputDigest: "sha256:00" },
    committedAt: "2026-09-12T00:00:00.000Z",
  });
}

function capabilities(state: Capability["state"], provision: CapabilityProvision): CapabilitySnapshot {
  // Start from the real snapshot so every CapabilityName is present, then
  // override only the one this projection reads.
  const snapshot = printCapabilities();
  snapshot.capabilities["completion-native-turn"] = {
    state,
    provision,
    scope: [],
    reasonCode: "test",
    prerequisites: [],
    evidence: [],
  };
  return snapshot;
}

describe("projectCommandStatus — the eight P0-3 rows", () => {
  it("row 1: queued and not dispatched → 等待发送", () => {
    expect(projectCommandStatus({ command: command("queued", "not-dispatched") })).toMatchObject({
      key: "awaiting-send",
      label: "等待发送",
      success: false,
    });
    expect(projectCommandStatus({ command: command("queued", "intent-durable") }).key).toBe("awaiting-send");
    // Local optimistic bubble, before any server command exists.
    expect(projectCommandStatus({ localState: "queued", hasServerCommandId: false }).key).toBe("awaiting-send");
  });

  it("row 1 offers cancel-unsent but never a resend", () => {
    const row = projectCommandStatus({ command: command("queued", "not-dispatched") });
    expect(row.actions).toContain("cancel-unsent");
    expect(row.actions).not.toContain("resend");
  });

  it("row 2: accepted → 已受理, and creating an instance → 会话已创建", () => {
    const accepted = projectCommandStatus({ command: command("accepted", "intent-durable") });
    expect(accepted).toMatchObject({ key: "accepted", label: "已受理", success: false });

    for (const lifecycle of ["requested", "preparing", "starting"] as Lifecycle[]) {
      expect(projectCommandStatus({ instance: instance({ lifecycle }) }).key).toBe("accepted");
    }
    expect(acceptedLabel({ instance: instance({ lifecycle: "starting" }) })).toBe("会话已创建");
    expect(acceptedLabel({ command: command("accepted", "intent-durable") })).toBe("已受理");
  });

  it("row 2 never reads as task success, even once natively acknowledged", () => {
    expect(projectCommandStatus({ command: command("accepted", "native-acknowledged") })).toMatchObject({
      key: "accepted",
      success: false,
    });
    // `settled` is a *command* outcome, not a business one.
    expect(projectCommandStatus({ command: command("settled", "native-acknowledged") }).success).toBe(false);
  });

  it("row 3: transport-written + clear → 已发送，等待确认", () => {
    expect(projectCommandStatus({ command: command("accepted", "transport-written", "clear") })).toMatchObject({
      key: "sent-awaiting-ack",
      label: "已发送，等待确认",
      success: false,
    });
  });

  it("row 3 offers no automatic resend of native input", () => {
    const row = projectCommandStatus({ command: command("accepted", "transport-written", "clear") });
    expect(row.actions).toEqual(["view"]);
  });

  it("row 4: every unknown / reconciling / disconnected fact → 状态待确认", () => {
    const expected = { key: "unconfirmed", label: "状态待确认", success: false };

    for (const resolution of ["unknown", "reconciling"] as Command["resolution"][]) {
      expect(projectCommandStatus({ command: command("accepted", "transport-written", resolution) })).toMatchObject(expected);
    }
    for (const connectivity of ["disconnected", "reconciling"] as Connectivity[]) {
      expect(projectCommandStatus({ instance: instance({ connectivity }) })).toMatchObject(expected);
    }
    for (const lifecycle of ["unknown", "reconciling"] as Lifecycle[]) {
      expect(projectCommandStatus({ instance: instance({ lifecycle }) })).toMatchObject(expected);
    }
    expect(projectCommandStatus({ instance: instance({ lastError: NODE_EPOCH_CHANGED }) })).toMatchObject(expected);
    expect(
      projectCommandStatus({ interaction: { interaction: interaction({ delivery: "unknown", state: "answer-committed", answer: answeredBy("dev_a") }), deviceId: "dev_a" }, host: host() }),
    ).toMatchObject(expected);
  });

  it("row 4 outranks an otherwise-sent command", () => {
    // Disconnected while the command claims transport-written: unknown wins.
    const row = projectCommandStatus({
      command: command("accepted", "transport-written", "clear"),
      instance: instance({ connectivity: "disconnected" }),
    });
    expect(row.key).toBe("unconfirmed");
  });

  it("row 4 offers refresh and copy-diagnostic only — refresh cannot re-send", () => {
    const row = projectCommandStatus({ command: command("accepted", "transport-written", "unknown") });
    expect(row.actions).toEqual(["refresh", "copy-diagnostic"]);
  });

  it("row 5: a pending answerable interaction → 需要你回答", () => {
    expect(projectCommandStatus({ interaction: { interaction: interaction() }, host: host() })).toMatchObject({
      key: "needs-answer",
      label: "需要你回答",
      success: false,
      actions: ["open-interaction"],
    });
  });

  it("row 5 covers every blocking interaction kind", () => {
    for (const kind of ["approval", "question", "plan-review", "elicitation"] as Interaction["kind"][]) {
      expect(projectCommandStatus({ interaction: { interaction: interaction({ kind }) }, host: host() }).key).toBe("needs-answer");
    }
  });

  it("row 6: answer committed with no native-cleared → 回答已提交，等待处理", () => {
    const committed = interaction({ state: "answer-committed", answer: answeredBy("dev_a"), delivery: "written" });
    expect(projectCommandStatus({ interaction: { interaction: committed, deviceId: "dev_a" }, host: host() })).toMatchObject({
      key: "answer-submitted",
      label: "回答已提交，等待处理",
      success: false,
    });
  });

  it("row 6: another device's answer is also 回答已提交，等待处理, not success", () => {
    const elsewhere = interaction({ state: "answer-committed", answer: answeredBy("dev_other"), delivery: "written" });
    expect(projectCommandStatus({ interaction: { interaction: elsewhere, deviceId: "dev_a" }, host: host() })).toMatchObject({
      key: "answer-submitted",
      success: false,
    });
  });

  it("row 7: complete + native completion capability → 本轮已结束", () => {
    for (const contentStatus of ["complete", "interrupted"] as const) {
      expect(
        projectCommandStatus({ turn: { contentStatus, capabilities: capabilities("supported", "native") } }),
      ).toMatchObject({ key: "turn-ended", label: "本轮已结束", success: true });
    }
  });

  it("row 7 degrades to 状态待确认 when the completion fact is emulated or unknown", () => {
    for (const provision of ["emulated", "unknown"] as CapabilityProvision[]) {
      const row = projectCommandStatus({ turn: { contentStatus: "complete", capabilities: capabilities("supported", provision) } });
      expect(row).toMatchObject({ key: "unconfirmed", success: false });
    }
    // Capability not supported at all.
    expect(projectCommandStatus({ turn: { contentStatus: "complete", capabilities: capabilities("unsupported", "native") } }).key).toBe("unconfirmed");
    // No capability snapshot to consult.
    expect(projectCommandStatus({ turn: { contentStatus: "complete", capabilities: null } }).key).toBe("unconfirmed");
  });

  it("row 7 is never inferred from a settled command or a streaming status", () => {
    // `Settled` is transport, not business success (plan §3).
    expect(projectCommandStatus({ command: command("settled", "native-acknowledged", "clear") }).success).toBe(false);
    for (const contentStatus of ["queued", "streaming", "unknown"] as const) {
      const row = projectCommandStatus({ turn: { contentStatus, capabilities: capabilities("supported", "native") } });
      expect(row.success).toBe(false);
    }
  });

  it("row 8: deleted with an unconfirmed purge → 会话记录已删除，主机数据待清理", () => {
    for (const nodePurge of ["node-offline", "node-rejected", "purge-failed"]) {
      expect(projectCommandStatus({ deletion: { nodePurge } })).toMatchObject({
        key: "record-deleted-purge-pending",
        label: "会话记录已删除，主机数据待清理",
        success: false,
      });
    }
    // Absent or unrecognised values are not confirmation either.
    expect(projectCommandStatus({ deletion: {} }).key).toBe("record-deleted-purge-pending");
    expect(projectCommandStatus({ deletion: { nodePurge: "something-new" } }).key).toBe("record-deleted-purge-pending");
  });

  it("row 8: a purged delete leaves no standing status", () => {
    expect(projectDeletion({ nodePurge: "purged" })).toBeNull();
    expect(projectDeletion({ nodePurge: "node-offline" })).toMatchObject({ key: "record-deleted-purge-pending" });
    expect(projectDeletion(null)).toBeNull();
  });

  it("row 8 outranks live-session facts — a deleted record is not a running turn", () => {
    const row = projectCommandStatus({
      deletion: { nodePurge: "node-offline" },
      command: command("accepted", "transport-written", "clear"),
      turn: { contentStatus: "complete", capabilities: capabilities("supported", "native") },
    });
    expect(row.key).toBe("record-deleted-purge-pending");
  });
});

describe("projectCommandStatus — unknown never renders as success", () => {
  it("no facts at all → 状态待确认", () => {
    expect(projectCommandStatus({})).toMatchObject({ key: "unconfirmed", success: false });
  });

  it("nulls and empty records → 状态待确认", () => {
    expect(projectCommandStatus({ command: null, instance: null, interaction: null, turn: null, deletion: null }).key).toBe("unconfirmed");
  });

  it("a local bubble without a server commandId cannot claim a delivered phase", () => {
    // `unknown` is exactly the store's failure path today (store.ts:545-547).
    expect(projectCommandStatus({ localState: "unknown", hasServerCommandId: false }).key).toBe("unconfirmed");
    expect(projectCommandStatus({ localState: "accepted", hasServerCommandId: false }).key).toBe("unconfirmed");
  });

  it("host offline pauses an interaction into 状态待确认, not an answer row", () => {
    const row = projectCommandStatus({ interaction: { interaction: interaction() }, host: host("offline") });
    expect(row).toMatchObject({ key: "unconfirmed", success: false });
  });

  it("an unknown-state interaction stays answerable rather than hiding behind 状态待确认", () => {
    // Deliberate, and inherited from `projectInteraction` rather than decided
    // here: an interaction the Hub is unsure about is still shown as
    // actionable while it is `answerable`, because the opposite failure —
    // hiding a request that really is blocking the agent, leaving nobody able
    // to unblock it — is worse than offering a form that turns out to be
    // stale. It is an attention row, never a success row.
    const row = projectCommandStatus({ interaction: { interaction: interaction({ state: "unknown" }) }, host: host() });
    expect(row).toMatchObject({ key: "needs-answer", success: false });

    // Once it is not answerable there is nothing to offer, so it degrades.
    const sealed = projectCommandStatus({
      interaction: { interaction: interaction({ state: "unknown", answerable: false }) },
      host: host(),
    });
    expect(sealed).toMatchObject({ key: "unconfirmed", success: false });
  });

  it("a natively-cleared interaction still needs its own turn fact to end the turn", () => {
    const cleared = interaction({
      state: "resolved",
      answer: answeredBy("dev_a"),
      delivery: "confirmed",
      resolution: known({ reason: "native-cleared", eventIds: [] }),
    });
    // Cleared, but nothing says the turn finished.
    expect(projectCommandStatus({ interaction: { interaction: cleared, deviceId: "dev_a" }, host: host() })).toMatchObject({
      key: "unconfirmed",
      success: false,
    });
    // With a real native completion fact it may end.
    expect(
      projectCommandStatus({
        interaction: { interaction: cleared, deviceId: "dev_a" },
        host: host(),
        turn: { contentStatus: "complete", capabilities: capabilities("supported", "native") },
      }),
    ).toMatchObject({ key: "turn-ended", success: true });
  });

  it("exactly one row is a success row", () => {
    const keys = Object.keys(COMMAND_STATUS_LABEL) as (keyof typeof COMMAND_STATUS_LABEL)[];
    expect(keys).toHaveLength(8);
    const successes = [
      projectCommandStatus({ command: command("queued", "not-dispatched") }),
      projectCommandStatus({ command: command("accepted", "intent-durable") }),
      projectCommandStatus({ command: command("accepted", "transport-written", "clear") }),
      projectCommandStatus({ command: command("accepted", "transport-written", "unknown") }),
      projectCommandStatus({ interaction: { interaction: interaction() }, host: host() }),
      projectCommandStatus({ interaction: { interaction: interaction({ state: "answer-committed", answer: answeredBy("d"), delivery: "written" }), deviceId: "d" }, host: host() }),
      projectCommandStatus({ turn: { contentStatus: "complete", capabilities: capabilities("supported", "native") } }),
      projectCommandStatus({ deletion: { nodePurge: "node-offline" } }),
    ].filter((r) => r.success);
    expect(successes.map((r) => r.key)).toEqual(["turn-ended"]);
  });

  it("no row offers a resend action", () => {
    const rows = [
      projectCommandStatus({ command: command("queued", "not-dispatched") }),
      projectCommandStatus({ command: command("accepted", "transport-written", "clear") }),
      projectCommandStatus({ command: command("accepted", "transport-written", "unknown") }),
      projectCommandStatus({ interaction: { interaction: interaction() }, host: host() }),
      projectCommandStatus({ deletion: { nodePurge: "purge-failed" } }),
    ];
    for (const row of rows) {
      expect(row.actions.join(",")).not.toMatch(/resend|retry|重发/);
    }
  });
});

describe("canSubmitAnswer", () => {
  it("a pending interaction is submittable", () => {
    expect(canSubmitAnswer(interaction(), { host: host() })).toBe(true);
  });

  it("answered but not natively cleared is NOT resubmittable", () => {
    const committed = interaction({ state: "answer-committed", answer: answeredBy("dev_a"), delivery: "written" });
    expect(canSubmitAnswer(committed, { deviceId: "dev_a", host: host() })).toBe(false);
  });

  it("a concurrent answer from another device blocks this device's submit", () => {
    const elsewhere = interaction({ state: "answer-committed", answer: answeredBy("dev_other"), delivery: "written" });
    expect(canSubmitAnswer(elsewhere, { deviceId: "dev_a", host: host() })).toBe(false);
  });

  it("an in-flight local submit blocks a second submit", () => {
    expect(canSubmitAnswer(interaction(), { answering: true, host: host() })).toBe(false);
  });

  it("non-answerable, expired, invalidated and offline are all unsubmittable", () => {
    expect(canSubmitAnswer(interaction({ answerable: false }), { host: host() })).toBe(false);
    expect(canSubmitAnswer(interaction({ state: "expired" }), { host: host() })).toBe(false);
    expect(canSubmitAnswer(interaction({ state: "invalidated" }), { host: host() })).toBe(false);
    expect(canSubmitAnswer(interaction(), { host: host("offline") })).toBe(false);
    expect(canSubmitAnswer(interaction(), { host: host(), connectivity: "disconnected" })).toBe(false);
  });
});
