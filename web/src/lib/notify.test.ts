import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BLOCKING_LIMIT,
  formatDiagnostic,
  INFO_LIMIT,
  INFO_TTL_MS,
  NotifyStore,
  type Notification,
} from "./notify";

let store: NotifyStore;

beforeEach(() => {
  vi.useFakeTimers();
  store = new NotifyStore();
});

afterEach(() => {
  store.reset();
  vi.useRealTimers();
});

function blocking(subject: string, stage = "删除") {
  return store.notify({ subject, stage, reason: "主机离线", severity: "blocking" as const });
}

describe("notify — severities have different lifetimes", () => {
  it("info lands in the transient list and expires on its own", () => {
    store.notify({ subject: "会话 alpha", stage: "保存", severity: "info" });
    expect(store.getState().info).toHaveLength(1);
    expect(store.getState().blocking).toHaveLength(0);

    vi.advanceTimersByTime(INFO_TTL_MS + 1);
    expect(store.getState().info).toHaveLength(0);
  });

  it("blocking lands in the standing list and never expires", () => {
    blocking("会话 alpha");
    expect(store.getState().blocking).toHaveLength(1);

    // Far beyond any info TTL.
    vi.advanceTimersByTime(INFO_TTL_MS * 100);
    expect(store.getState().blocking).toHaveLength(1);
  });

  it("a blocking notification is only removed by an explicit dismiss", () => {
    const id = blocking("会话 alpha");
    vi.advanceTimersByTime(INFO_TTL_MS * 10);
    expect(store.getState().blocking).toHaveLength(1);

    store.dismiss(id);
    expect(store.getState().blocking).toHaveLength(0);
  });
});

describe("notify — a later success never covers a blocking error", () => {
  it("the error survives a subsequent info notification", () => {
    blocking("会话 alpha", "删除");
    store.notify({ subject: "会话 beta", stage: "保存", severity: "info" });

    const state = store.getState();
    expect(state.blocking).toHaveLength(1);
    expect(state.blocking[0].subject).toBe("会话 alpha");
    expect(state.info).toHaveLength(1);
  });

  it("the error survives after the success has expired", () => {
    blocking("会话 alpha");
    store.notify({ subject: "会话 beta", stage: "保存", severity: "info" });

    vi.advanceTimersByTime(INFO_TTL_MS + 1);
    expect(store.getState().info).toHaveLength(0);
    expect(store.getState().blocking).toHaveLength(1);
  });

  it("a burst of successes cannot evict a standing error", () => {
    blocking("会话 alpha");
    for (let i = 0; i < INFO_LIMIT * 5; i++) {
      store.notify({ subject: `会话 ${i}`, stage: "保存", severity: "info" });
    }
    expect(store.getState().info.length).toBeLessThanOrEqual(INFO_LIMIT);
    expect(store.getState().blocking).toHaveLength(1);
  });

  it("info and blocking live in separate lists, so capping one cannot drop the other", () => {
    blocking("会话 alpha");
    for (let i = 0; i < 20; i++) store.notify({ subject: `s${i}`, stage: "保存", severity: "info" });
    expect(store.getState().blocking.map((n) => n.subject)).toEqual(["会话 alpha"]);
  });

  it("a success on the same subject and stage does not replace the error", () => {
    // Same subject/stage, different severity — the default key includes
    // severity precisely so a success cannot collapse onto an error.
    blocking("会话 alpha", "删除");
    store.notify({ subject: "会话 alpha", stage: "删除", severity: "info" });
    expect(store.getState().blocking).toHaveLength(1);
    expect(store.getState().info).toHaveLength(1);
  });
});

describe("notify — collapsing and caps", () => {
  it("a repeat with the same key replaces rather than stacks", () => {
    store.notify({ subject: "会话 alpha", stage: "删除", reason: "第一次", severity: "blocking" });
    store.notify({ subject: "会话 alpha", stage: "删除", reason: "第二次", severity: "blocking" });

    const { blocking: list } = store.getState();
    expect(list).toHaveLength(1);
    expect(list[0].reason).toBe("第二次");
  });

  it("an explicit key collapses entries that differ in subject", () => {
    store.notify({ subject: "会话 a", stage: "同步", severity: "info", key: "sync" });
    store.notify({ subject: "会话 b", stage: "同步", severity: "info", key: "sync" });
    expect(store.getState().info).toHaveLength(1);
    expect(store.getState().info[0].subject).toBe("会话 b");
  });

  it("distinct subjects stack up to the caps, keeping the newest", () => {
    for (let i = 0; i < INFO_LIMIT + 2; i++) {
      store.notify({ subject: `会话 ${i}`, stage: "保存", severity: "info" });
    }
    const info = store.getState().info;
    expect(info).toHaveLength(INFO_LIMIT);
    expect(info[info.length - 1].subject).toBe(`会话 ${INFO_LIMIT + 1}`);

    for (let i = 0; i < BLOCKING_LIMIT + 2; i++) blocking(`错误 ${i}`);
    expect(store.getState().blocking).toHaveLength(BLOCKING_LIMIT);
  });

  it("re-notifying a key restarts its expiry instead of leaving a stale timer", () => {
    store.notify({ subject: "会话 alpha", stage: "保存", severity: "info" });
    vi.advanceTimersByTime(INFO_TTL_MS - 100);
    // Refresh the same key just before it would have expired.
    store.notify({ subject: "会话 alpha", stage: "保存", severity: "info" });

    vi.advanceTimersByTime(200);
    // The original timer must not have taken the refreshed entry with it.
    expect(store.getState().info).toHaveLength(1);

    vi.advanceTimersByTime(INFO_TTL_MS);
    expect(store.getState().info).toHaveLength(0);
  });

  it("an entry pushed out by the cap does not take a later one with it", () => {
    // Each info gets its own expiry timer. When the cap evicts an entry early,
    // its timer still fires later — it must not remove whatever took its slot.
    for (let i = 0; i < INFO_LIMIT + 1; i++) {
      store.notify({ subject: `会话 ${i}`, stage: "保存", severity: "info" });
      vi.advanceTimersByTime(10);
    }
    expect(store.getState().info).toHaveLength(INFO_LIMIT);

    // Let the evicted entry's timer fire, but not the survivors'.
    vi.advanceTimersByTime(INFO_TTL_MS - 100);
    expect(store.getState().info).toHaveLength(INFO_LIMIT);
  });

  it("dismissAllBlocking clears the standing area only", () => {
    blocking("错误 a");
    blocking("错误 b");
    store.notify({ subject: "会话", stage: "保存", severity: "info" });

    store.dismissAllBlocking();
    expect(store.getState().blocking).toHaveLength(0);
    expect(store.getState().info).toHaveLength(1);
  });
});

describe("notify — text, actions and subscribers", () => {
  it("renders 对象 · 阶段 · 原因, omitting an absent reason", () => {
    store.notify({ subject: "会话 alpha", stage: "删除", reason: "主机离线", severity: "blocking" });
    expect(store.getState().blocking[0].text).toBe("会话 alpha · 删除 · 主机离线");

    store.notify({ subject: "会话 beta", stage: "保存", severity: "info" });
    expect(store.getState().info[0].text).toBe("会话 beta · 保存");
  });

  it("carries actions through and can run them", async () => {
    const run = vi.fn();
    store.notify({
      subject: "会话 alpha",
      stage: "删除",
      severity: "blocking",
      actions: [
        { id: "view", label: "查看", run },
        { id: "refresh", label: "刷新", run: () => {} },
      ],
    });

    const actions = store.getState().blocking[0].actions ?? [];
    expect(actions.map((a) => a.label)).toEqual(["查看", "刷新"]);
    await actions[0].run?.();
    expect(run).toHaveBeenCalledOnce();
  });

  it("notifies subscribers and stops after unsubscribe", () => {
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);

    store.notify({ subject: "a", stage: "保存", severity: "info" });
    expect(listener).toHaveBeenCalledTimes(1);

    unsubscribe();
    store.notify({ subject: "b", stage: "保存", severity: "info" });
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("returns an id that identifies the entry", () => {
    const first = blocking("错误 a");
    const second = blocking("错误 b");
    expect(first).not.toBe(second);

    store.dismiss(first);
    expect(store.getState().blocking.map((n) => n.id)).toEqual([second]);
  });
});

describe("formatDiagnostic — sanitised, necessary fields only", () => {
  function notification(patch: Partial<Notification> = {}): Notification {
    return {
      id: "ntf_1",
      subject: "会话 alpha",
      stage: "删除",
      severity: "blocking",
      createdAt: 0,
      text: "会话 alpha · 删除",
      ...patch,
    };
  }

  it("includes the identifying fields that are present", () => {
    const text = formatDiagnostic(
      notification({
        reason: "主机离线",
        diagnostic: {
          instanceId: "ins_1",
          hostId: "hst_1",
          reasonCode: "node-epoch-changed",
          httpStatus: 409,
          statusKey: "unconfirmed",
          at: "2026-09-14T00:00:00.000Z",
        },
      }),
    );

    expect(text).toContain("会话 alpha · 删除");
    expect(text).toContain("reason: 主机离线");
    expect(text).toContain("instanceId: ins_1");
    expect(text).toContain("hostId: hst_1");
    expect(text).toContain("reasonCode: node-epoch-changed");
    expect(text).toContain("httpStatus: 409");
    expect(text).toContain("statusKey: unconfirmed");
  });

  it("omits absent, null and empty fields instead of printing blanks", () => {
    const text = formatDiagnostic(
      notification({ diagnostic: { instanceId: "ins_1", hostId: null, commandId: "", reasonCode: undefined } }),
    );
    expect(text).toContain("instanceId: ins_1");
    expect(text).not.toContain("hostId");
    expect(text).not.toContain("commandId");
    expect(text).not.toContain("reasonCode");
    expect(text).not.toMatch(/:\s*$/m);
  });

  it("copies nothing when there is nothing but the headline", () => {
    expect(formatDiagnostic(notification())).toBe("会话 alpha · 删除");
  });

  it("cannot be made to leak fields outside the allow-list", () => {
    const text = formatDiagnostic(
      notification({
        diagnostic: {
          instanceId: "ins_1",
          // Fields a careless caller might attach. The formatter walks a
          // fixed field list, so none of these can reach the clipboard.
          prompt: "secret user prompt",
          token: "tok_live_abc123",
          transcript: "line one\nline two",
          cwd: "/home/someone/project",
        } as never,
      }),
    );

    expect(text).toContain("instanceId: ins_1");
    expect(text).not.toContain("secret user prompt");
    expect(text).not.toContain("tok_live_abc123");
    expect(text).not.toContain("line one");
    expect(text).not.toContain("/home/someone/project");
  });
});
