import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import type { Interaction, InteractionRequest } from "../../types/interaction";
import { nextStep, type RowScreen } from "./nextStep";

function instance(patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id: "ins_next" as Id,
    hostId: "host-a" as Id,
    workspaceId: "wsp-a" as Id,
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    parent: null,
    exit: { state: "not-applicable" },
    ...patch,
  } as Instance;
}

/** Print-carrier mock caps advertise resume; flip a single capability by name. */
function withCapability(
  source: Instance,
  name: "resume",
  state: "supported" | "unsupported" | "unknown",
): Instance {
  return {
    ...source,
    capabilities: {
      ...source.capabilities,
      capabilities: {
        ...source.capabilities.capabilities,
        [name]: {
          state,
          scope: [],
          reasonCode: state === "supported" ? "resume" : "no-transcript",
          prerequisites: [],
          evidence: [],
        },
      },
    },
  };
}

function pending(request: InteractionRequest): Interaction {
  return {
    id: "int_next" as Id,
    revision: "1",
    createdAt: "2026-09-19T00:00:00.000Z",
    updatedAt: "2026-09-19T00:00:00.000Z",
    instanceId: "ins_next" as Id,
    runId: null,
    hostId: "host-a" as Id,
    kind: request.kind,
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "ask" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "host-a" as Id,
    },
    requestVersion: "1",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request,
    deadline: { state: "unknown", reason: "none", evidenceEventIds: [] },
    deadlineSource: "none",
    answer: { state: "not-applicable" },
    delivery: "not-sent",
    resolution: { state: "not-applicable" },
  };
}

const approval = pending({
  kind: "approval",
  title: "Bash",
  description: "rm -rf /tmp/coord-media",
  toolCallId: null,
  actionRef: "obj_1" as Id,
  options: [],
  requestedPermissionsRef: null,
  inputDigest: "sha256:00",
});

const question = pending({
  kind: "question",
  title: "AskUserQuestion",
  fields: [
    {
      id: "q0",
      title: "接下来想做什么？",
      description: null,
      input: "single-select",
      required: true,
      options: [{ id: "a", label: "继续" }],
      allowFreeText: true,
      sensitive: false,
    },
    {
      id: "q1",
      title: "保存吗？",
      description: null,
      input: "single-select",
      required: true,
      options: [{ id: "b", label: "保存" }],
      allowFreeText: false,
      sensitive: false,
    },
  ],
});

const planReview = pending({
  kind: "plan-review",
  title: "重构方案 v2",
  planRef: "obj_plan" as Id,
  planRevision: "3",
  planDigest: "sha256:plan",
  options: [],
  allowFeedback: true,
});

const elicitation = pending({
  kind: "elicitation",
  title: "补充参数",
  mode: "form",
  schemaRef: "obj_schema" as Id,
  schemaDialect: "json-schema",
  url: null,
  nativeExtension: null,
  allowedActions: ["accept", "decline", "cancel"],
});

const screenDone: RowScreen = { lines: ["DONE 8d3144d7"], done: true };

describe("nextStep — six projected states", () => {
  it("blocked with a pending approval quotes the request description, not the wire activity", () => {
    const step = nextStep(
      instance({ activity: known("waiting-interaction") }),
      approval,
    );
    expect(step.tone).toBe("blocked");
    expect(step.text).toBe("rm -rf /tmp/coord-media");
    expect(step.text).not.toContain("waiting-interaction");
  });

  it("blocked with a pending question names the form and the field count", () => {
    const step = nextStep(instance({ activity: known("waiting-interaction") }), question);
    expect(step.tone).toBe("blocked");
    expect(step.text).toBe("AskUserQuestion · 2 题待回答");
  });

  it("blocked with a pending plan review names the plan", () => {
    const step = nextStep(instance({ activity: known("waiting-interaction") }), planReview);
    expect(step).toEqual({ text: "计划待审 · 重构方案 v2", tone: "blocked", detail: null });
  });

  it("blocked with a pending elicitation names the form", () => {
    const step = nextStep(instance({ activity: known("waiting-interaction") }), elicitation);
    expect(step).toEqual({ text: "待处理表单 · 补充参数", tone: "blocked", detail: null });
  });

  it("blocked without a loaded interaction still says what to do", () => {
    const step = nextStep(instance({ activity: known("waiting-interaction") }), null);
    expect(step).toEqual({ text: "等待处理交互", tone: "blocked", detail: null });
  });

  it("working says just that with no known phrase, and uses the live phrase when one is projected", () => {
    const running = instance({ activity: known("working") });
    expect(nextStep(running)).toEqual({ text: "运行中…", tone: "working", detail: null });
    const phrased = nextStep(running, null, undefined, "Workflow wf_ab12 · phase compile");
    expect(phrased).toEqual({ text: "Workflow wf_ab12 · phase compile", tone: "working", detail: null });
    // Blank/whitespace phrases are treated as "nothing known", never invented.
    expect(nextStep(running, null, undefined, "   ")).toEqual({ text: "运行中…", tone: "working", detail: null });
    const marked = nextStep(running, null, screenDone);
    expect(marked.tone).toBe("working");
    expect(marked.text).toContain("DONE");
    const markedPhrase = nextStep(running, null, screenDone, "ninja build");
    expect(markedPhrase.text).toBe("终端已打出 DONE · ninja build");
  });

  it("idle says the turn ended while the process stays alive", () => {
    expect(nextStep(instance({ activity: known("idle") }))).toEqual({
      text: "回合结束、进程仍在 · 可继续发送",
      tone: "idle",
      detail: null,
    });
  });

  it("starting reports launch without spelling out the lifecycle wire value", () => {
    const step = nextStep(
      instance({ lifecycle: "starting", activity: known("idle") }),
    );
    expect(step.tone).toBe("starting");
    expect(step.text).toContain("正在拉起");
    expect(step.text).not.toContain("lifecycle");
    expect(step.text).not.toContain("starting");
  });

  it("exited routes its sentence/tone through endReason and keeps the raw code in detail", () => {
    const crashed = instance({
      lifecycle: "failed",
      activity: known("idle"),
      exit: known({ code: 1, signal: null, observedAt: "2026-09-19T00:00:00Z" }),
    });
    const coded = nextStep(withCapability(crashed, "resume", "supported"));
    expect(coded.tone).toBe("failed");
    expect(coded.text).toBe("会话异常退出（exit 1） · 可恢复");
    expect(coded.detail).toBeNull();

    const noCode = nextStep(
      withCapability(
        instance({ lifecycle: "exited", activity: known("idle"), exit: { state: "unknown", reason: "none", evidenceEventIds: [] } }),
        "resume",
        "supported",
      ),
    );
    expect(noCode.text).toBe("已结束 · 可恢复");
    expect(noCode.tone).toBe("ended");

    const restarted = instance({
      lifecycle: "exited",
      activity: known("idle"),
      lastError: "node-epoch-changed",
    });
    const restart = nextStep(withCapability(restarted, "resume", "supported"));
    expect(restart.text).toBe("Node 重启，会话已中断 · 可恢复");
    expect(restart.tone).toBe("interrupted");
    expect(restart.detail).toBe("node-epoch-changed");
  });

  it("split crash vs termination signals on the exited sentence", () => {
    const segv = nextStep(
      withCapability(
        instance({
          lifecycle: "failed",
          activity: known("idle"),
          lastError: "native-exit-signal-SIGSEGV",
        }),
        "resume",
        "unsupported",
      ),
    );
    expect(segv.text).toBe("进程崩溃（SIGSEGV）");
    expect(segv.tone).toBe("failed");

    const term = nextStep(
      withCapability(
        instance({
          lifecycle: "failed",
          activity: known("idle"),
          lastError: "native-exit-signal-SIGTERM",
        }),
        "resume",
        "unsupported",
      ),
    );
    expect(term.text).toBe("进程被终止（SIGTERM），会话已中断");
    expect(term.tone).toBe("interrupted");
  });

  it("exited promises recovery only while the resume capability is supported", () => {
    const exited = instance({
      lifecycle: "exited",
      activity: known("idle"),
      exit: known({ code: 0, signal: null, observedAt: "2026-09-19T00:00:00Z" }),
    });
    expect(nextStep(withCapability(exited, "resume", "unsupported"))).toMatchObject({
      text: "已结束",
      tone: "ended",
    });
    expect(nextStep(withCapability(exited, "resume", "unknown"))).toMatchObject({
      text: "已结束",
      tone: "ended",
    });
    expect(nextStep(withCapability(exited, "resume", "supported"))).toMatchObject({
      text: "已结束 · 可恢复",
      tone: "ended",
    });
  });

  it("unknown never borrows idle or working copy", () => {
    const step = nextStep(instance({ lifecycle: "unknown" }));
    expect(step.tone).toBe("unknown");
    expect(step.text).toContain("状态待确认");
    expect(step.text).not.toContain("空闲");
    expect(step.text).not.toContain("运行中");
  });
});

describe("nextStep — precedence", () => {
  it("a disconnected host projects unknown even while activity claims working", () => {
    const step = nextStep(
      instance({ activity: known("working"), connectivity: "disconnected" }),
    );
    expect(step.tone).toBe("unknown");
    expect(step.text).toContain("状态待确认");
  });

  it("a pending interaction on a disconnected row still loses to unknown", () => {
    // The dot and sentence must agree: connectivity beats a stale interaction.
    const step = nextStep(
      instance({ activity: known("waiting-interaction"), connectivity: "reconciling" }),
      approval,
    );
    expect(step.tone).toBe("unknown");
    expect(step.text).not.toContain("rm -rf");
  });

  it("a resolved (non-pending) interaction is ignored", () => {
    const answered = { ...approval, state: "resolved" as const };
    expect(nextStep(instance({ activity: known("idle") }), answered).tone).toBe("idle");
  });
});
