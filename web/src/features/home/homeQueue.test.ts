import { describe, expect, it } from "vitest";
import { glanceCounts } from "./glance";
import {
  HOME_QUEUE_LIMIT,
  chipAnswer,
  deriveHomeQueue,
  failedTurnCards,
  homeApprovalOptions,
  homeCardKind,
  inboxHref,
  isSimpleQuestion,
  lastLines,
  noticeCards,
} from "./homeQueue";
import {
  APPROVAL,
  PLAN,
  T0,
  T1,
  T2,
  field,
  inst,
  interaction,
  notice,
  question,
  source,
  task,
} from "./queueTestData";

/**
 * home.md §4.1–4.6 and §5.1: the queue's pure rules. Every card-kind rule of
 * §4.2 has a case here, including the two that must never be weakened:
 * allow-session is never a home option, and a plan is never approvable.
 */

describe("homeCardKind (§4.2)", () => {
  it("classifies an approval and offers exactly deny and allow-once, never allow-session", () => {
    const item = interaction({ id: "int_a", instanceId: "ins_a" });
    expect(homeCardKind(item)).toBe("approval");
    const options = homeApprovalOptions(item);
    expect(options?.deny.effect).toBe("deny");
    expect(options?.allowOnce.effect).toBe("allow-once");
    expect(Object.values(options ?? {}).some((opt) => opt.effect === "allow-session")).toBe(false);
  });

  it("offers nothing (去收件箱 only) when either deny or allow-once is missing", () => {
    const onlySession = interaction({
      id: "int_a",
      instanceId: "ins_a",
      request: {
        ...APPROVAL,
        options: APPROVAL.options.filter((opt) => opt.effect !== "allow-once"),
      },
    });
    expect(homeCardKind(onlySession)).toBe("approval");
    expect(homeApprovalOptions(onlySession)).toBeNull();
    const noDeny = interaction({
      id: "int_b",
      instanceId: "ins_a",
      request: {
        ...APPROVAL,
        options: APPROVAL.options.filter((opt) => opt.effect !== "deny"),
      },
    });
    expect(homeApprovalOptions(noDeny)).toBeNull();
  });

  it("classifies a single-field single-select question with ≤4 options as a chip question", () => {
    const item = interaction({ id: "int_q", instanceId: "ins_a", request: question() });
    expect(isSimpleQuestion(item)).toBe(true);
    expect(homeCardKind(item)).toBe("simple-question");
    expect(chipAnswer(item, "保留")).toEqual({
      kind: "question",
      answers: { q0: { optionIds: ["保留"], text: null } },
    });
    expect(chipAnswer(item, "nope")).toBeNull();
  });

  it("sends every other question to 去回答", () => {
    const five = field({
      options: ["a", "b", "c", "d", "e"].map((id) => ({ id, label: id })),
    });
    const cases = [
      question([five]),
      question([field({ input: "multi-select" })]),
      question([field({ input: "text", options: [] })]),
      question([field(), field({ id: "q1" })]),
    ];
    for (const request of cases) {
      const item = interaction({ id: "int_q", instanceId: "ins_a", request });
      expect(isSimpleQuestion(item)).toBe(false);
      expect(homeCardKind(item)).toBe("question");
    }
  });

  it("never gives a sensitive field a chip, and an elicitation is 去会话 only", () => {
    const sensitive = interaction({
      id: "int_s",
      instanceId: "ins_a",
      request: question([field({ sensitive: true })]),
    });
    expect(homeCardKind(sensitive)).toBe("sensitive");
    expect(chipAnswer(sensitive, "删除")).toBeNull();
    const elicitation = interaction({
      id: "int_e",
      instanceId: "ins_a",
      request: {
        kind: "elicitation",
        title: "Sign in",
        mode: "form",
        schemaRef: null,
        schemaDialect: null,
        url: null,
        nativeExtension: null,
        allowedActions: ["accept", "decline"],
      },
    });
    expect(homeCardKind(elicitation)).toBe("sensitive");
  });

  it("classifies a plan as review-only: it never yields an approve option", () => {
    const item = interaction({ id: "int_p", instanceId: "ins_a", request: PLAN });
    expect(homeCardKind(item)).toBe("plan");
    // The approval helper is the only home source of answer buttons; a plan
    // review never reaches it.
    expect(homeApprovalOptions(item)).toBeNull();
    expect(chipAnswer(item, "approve")).toBeNull();
  });

  it("classifies a native-tty prompt without structured options as a terminal prompt", () => {
    const bare = interaction({
      id: "int_t",
      instanceId: "ins_a",
      carrier: "native-tty",
      request: question([field({ input: "text", options: [], description: "Continue? (y/n)" })]),
    });
    expect(homeCardKind(bare)).toBe("tty");
    const unanswerable = interaction({
      id: "int_u",
      instanceId: "ins_a",
      carrier: "native-tty",
      answerable: false,
    });
    expect(homeCardKind(unanswerable)).toBe("tty");
    // A native-tty single-select with ≤4 options is still a chip question.
    const keyed = interaction({ id: "int_k", instanceId: "ins_a", carrier: "native-tty", request: question() });
    expect(homeCardKind(keyed)).toBe("simple-question");
  });

  it("keeps the last three non-blank screen lines", () => {
    expect(lastLines(["a", "", "b", "c", "  ", "d"])).toEqual(["b", "c", "d"]);
  });
});

describe("deriveHomeQueue (§4.1 order, §4.6 dedupe, §5.1 count)", () => {
  const blockedTask = task({ id: "tsk_b", blockedReason: "等你确认验收范围", createdAt: T0 });
  const dedupedTask = task({ id: "tsk_d", blockedReason: "等你看 gate", createdAt: T0 });

  function fixture() {
    return source({
      instances: [
        inst({ id: "ins_a" }),
        inst({ id: "ins_b" }),
        inst({ id: "ins_task", taskId: "tsk_d" }),
        inst({
          id: "ins_fail",
          lastTurnError: { at: T2, text: "API Error: 529 overloaded" },
          unseen: true,
          turnEndSeq: "9",
          seenSeq: "3",
        }),
      ],
      interactions: [
        interaction({ id: "int_old", instanceId: "ins_a", createdAt: T0 }),
        interaction({ id: "int_new", instanceId: "ins_b", createdAt: T2, request: question() }),
        interaction({ id: "int_task", instanceId: "ins_task", createdAt: T1, request: PLAN }),
      ],
      tasks: [blockedTask, dedupedTask, task({ id: "tsk_arch", blockedReason: "x", archivedAt: T1 })],
    });
  }

  it("④ holds no item routed to Main or another agent; its task card shows instead (D-061)", () => {
    const src = fixture();
    src.interactions = src.interactions.map((row) =>
      row.id === "int_task" ? { ...row, routedTo: "main" as const } : row.id === "int_old" ? { ...row, routedTo: "agent" as const } : row,
    );
    const queue = deriveHomeQueue(src);
    expect(queue.cards.map((card) => `${card.type}:${card.id}`)).toEqual([
      "interaction:int_new",
      "task:tsk_b",
      "task:tsk_d",
      "failed:ins_fail",
    ]);
    // The header and the badge read the same N.
    expect(queue.counted).toBe(glanceCounts(src).needYou);
    expect(queue.counted).toBe(3);
  });

  it("orders interactions like the inbox (newest first), then 任务待你, then 回合失败", () => {
    const queue = deriveHomeQueue(fixture());
    expect(queue.cards.map((card) => `${card.type}:${card.id}`)).toEqual([
      "interaction:int_new",
      "interaction:int_task",
      "interaction:int_old",
      "task:tsk_b",
      "failed:ins_fail",
    ]);
  });

  it("puts 告诉你 after 任务待你 and before 回合失败, newest first, and never counts it (§4.1 kind 4)", () => {
    const src = fixture();
    const notices = [
      notice({ noticeId: "ntc_old", createdAt: T0 }),
      notice({ noticeId: "ntc_new", createdAt: T2 }),
      notice({ noticeId: "ntc_acked", createdAt: T2, ackedAt: T2 }),
    ];
    const queue = deriveHomeQueue(src, notices);
    expect(queue.cards.map((card) => `${card.type}:${card.id}`)).toEqual([
      "interaction:int_new",
      "interaction:int_task",
      "interaction:int_old",
      "task:tsk_b",
      "notice:ntc_new",
      "notice:ntc_old",
      "failed:ins_fail",
    ]);
    // Not in 「需要你」: the count is the same with or without notices.
    expect(queue.counted).toBe(deriveHomeQueue(src).counted);
    expect(queue.counted).toBe(glanceCounts(src).needYou);
    expect(noticeCards([notice({ noticeId: "ntc_x", ackedAt: T1 })])).toEqual([]);
  });

  it("drops a blocked task whose session already has an interaction card, and archived tasks", () => {
    const ids = deriveHomeQueue(fixture()).cards.map((card) => card.id);
    expect(ids).not.toContain("tsk_d");
    expect(ids).not.toContain("tsk_arch");
  });

  it("counts interactions and 任务待你 only, and equals glanceCounts().needYou", () => {
    const src = fixture();
    const queue = deriveHomeQueue(src);
    expect(queue.counted).toBe(4);
    expect(glanceCounts(src).needYou).toBe(queue.counted);
    // A source with nothing waiting: both read zero.
    const empty = source();
    expect(deriveHomeQueue(empty).counted).toBe(0);
    expect(glanceCounts(empty).needYou).toBe(0);
  });

  it("points 去收件箱 at the earliest interaction and omits it without interactions", () => {
    expect(deriveHomeQueue(fixture()).earliestInteractionId).toBe("int_old");
    expect(inboxHref("phone", "int_old")).toBe("/m/inbox?focus=int_old");
    expect(inboxHref("desktop", "int_old")).toBe("/approvals?focus=int_old");
    const tasksOnly = source({ tasks: [blockedTask] });
    expect(deriveHomeQueue(tasksOnly).earliestInteractionId).toBeNull();
  });

  it("leaves out ghost cards past their deadline, like the inbox and the count", () => {
    const src = source({
      instances: [inst({ id: "ins_a" })],
      interactions: [
        interaction({
          id: "int_ghost",
          instanceId: "ins_a",
          deadline: { state: "known", value: T0 } as never,
        }),
      ],
    });
    expect(deriveHomeQueue(src).cards).toEqual([]);
    expect(glanceCounts(src).needYou).toBe(0);
  });

  it("keeps the collapsed sizes of §4.1", () => {
    expect(HOME_QUEUE_LIMIT).toEqual({ phone: 3, desktop: 4 });
  });
});

describe("failed-turn cards (§4.5 after seen-hub)", () => {
  it("shows a failed turn only while it is unseen, and never counts it", () => {
    const failed = inst({ id: "ins_f", lastTurnError: { at: T1, text: "boom" }, unseen: true });
    const seen = inst({ id: "ins_s", lastTurnError: { at: T1, text: "boom" }, unseen: false });
    const ok = inst({ id: "ins_ok", unseen: true });
    expect(failedTurnCards([failed, seen, ok]).map((card) => card.id)).toEqual(["ins_f"]);
    const src = source({ instances: [failed] });
    expect(deriveHomeQueue(src).cards).toHaveLength(1);
    expect(deriveHomeQueue(src).counted).toBe(0);
    expect(glanceCounts(src).needYou).toBe(0);
  });

  it("orders failed cards newest failure first", () => {
    const a = inst({ id: "ins_a", lastTurnError: { at: T0, text: "a" }, unseen: true });
    const b = inst({ id: "ins_b", lastTurnError: { at: T2, text: "b" }, unseen: true });
    expect(failedTurnCards([a, b]).map((card) => card.id)).toEqual(["ins_b", "ins_a"]);
  });
});
