import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import type { LineageRecord } from "../../lib/api";
import type { LocalBubble, MainEntry } from "../../lib/store";
import type { Host, Instance } from "../../types/instance";
import type { Observation } from "../../types/observation";
import { known, type Id } from "../../types/wire";
import {
  firstLine,
  lineagePausedBy,
  mainEcho,
  mainStatus,
  readMainTail,
  restartsIn24h,
} from "./mainStatus";

const NOW = Date.parse("2026-10-30T12:00:00Z");

function entry(patch: Partial<Instance> = {}, live = true, generation = 1): MainEntry {
  const instance = {
    ...mockDb.instances[0],
    id: "ins_main" as Id,
    hostId: "host-a" as Id,
    lifecycle: "running",
    connectivity: "connected",
    activity: known("idle"),
    updatedAt: "2026-10-30T11:58:00Z",
    generation,
    grants: ["address-owner"],
    ...patch,
  } as Instance;
  return { instance, lineageId: "ins_main" as Id, generation, live, chapterIds: ["ins_main" as Id] };
}

function lineage(patch: Partial<LineageRecord> = {}): LineageRecord {
  return {
    lineageId: "ins_main" as Id,
    state: "running",
    generation: 1,
    chapters: [{ instanceId: "ins_main" as Id, generation: 1, createdAt: "2026-10-30T08:00:00Z" }],
    ...patch,
  };
}

const status = (e: MainEntry, extra: Partial<Parameters<typeof mainStatus>[0]> = {}) =>
  mainStatus({ entry: e, hostName: "devbox", asking: false, nowMs: NOW, ...extra });

describe("mainStatus line-1 phrase", () => {
  it("names a device pause with its time and turns the input into 恢复", () => {
    const s = status(entry(), {
      lineage: lineage({ state: "paused", pausedBy: { by: "device", at: "2026-10-30T11:59:40Z" } }),
    });
    expect(s.phase).toBe("paused");
    expect(s.phrase).toBe("已由你暂停 · 刚刚");
    expect(s.resumable).toBe(true);
  });

  it("an ended chapter with no paused-by record reads 已停止, names no actor, and offers 恢复", () => {
    const s = status(entry({ lifecycle: "exited" }, false), { lineage: lineage({ state: "starting" }) });
    expect(s.phase).toBe("stopped");
    expect(s.phrase.startsWith("已停止")).toBe(true);
    expect(s.phrase).not.toContain("你");
    expect(s.resumable).toBe(true);
  });

  it("host offline wins over activity and is amber", () => {
    const host = { id: "host-a", state: "offline", online: false, lastSeenAt: "2026-10-30T10:00:00Z" } as unknown as Host;
    const s = status(entry({ activity: known("working") }), { host });
    expect(s.phase).toBe("host-offline");
    expect(s.phrase).toBe("主机 devbox 离线 · 2h");
    expect(s.tone).toBe("attention");
    expect(s.hostOffline).toBe(true);
    expect(s.resumable).toBe(false);
  });

  it("a pending interaction reads 在问你", () => {
    expect(status(entry(), { asking: true }).phrase).toBe("在问你");
  });

  it("working carries the live phrase; idle carries the clock", () => {
    expect(status(entry({ activity: known("working") }), { summary: "在看 gate" }).phrase).toBe("工作中 · 在看 gate");
    expect(status(entry()).phrase.startsWith("空闲 · ")).toBe(true);
  });

  it("a starting successor after a non-owner cause reads 正在重启", () => {
    const e = entry({ lifecycle: "starting", chapterCause: "process-exit" }, true, 2);
    expect(status(e).phrase).toBe("正在重启");
    const resumed = entry({ lifecycle: "starting", chapterCause: "owner-resume" }, true, 2);
    expect(status(resumed).phrase).toBe("正在启动");
  });
});

describe("lineage read helpers", () => {
  it("feature-detects the paused-by record and never invents one", () => {
    expect(lineagePausedBy(lineage())).toBeNull();
    expect(lineagePausedBy(lineage({ pausedBy: "restart-cap" }))).toEqual({ by: "restart-cap", at: null });
    expect(lineagePausedBy(lineage({ state: "paused" }))).toEqual({ by: null, at: null });
  });

  it("counts only C1 restarts in the last 24 h, with causes; null before the record is read", () => {
    expect(restartsIn24h(null, NOW)).toBeNull();
    const record = lineage({
      chapters: [
        { instanceId: "a" as Id, generation: 1, createdAt: "2026-10-28T00:00:00Z" },
        { instanceId: "b" as Id, generation: 2, chapterCause: "start-failed", createdAt: "2026-10-28T01:00:00Z" },
        { instanceId: "c" as Id, generation: 3, chapterCause: "process-exit", createdAt: "2026-10-30T09:00:00Z" },
        { instanceId: "d" as Id, generation: 4, chapterCause: "owner-resume", createdAt: "2026-10-30T10:00:00Z" },
        { instanceId: "e" as Id, generation: 5, chapterCause: "node-restarted", createdAt: "2026-10-30T10:30:00Z" },
        { instanceId: "f" as Id, generation: 6, chapterCause: "some-new-cause", createdAt: "2026-10-30T11:00:00Z" },
      ],
    });
    // The 10-28 restart is older than 24 h; owner-resume is the owner's own
    // action, and an unknown spelling is never counted as a restart.
    expect(restartsIn24h(record, NOW)).toEqual({
      count: 2,
      causes: [
        { cause: "进程退出", count: 1 },
        { cause: "Node 重启", count: 1 },
      ],
    });
  });

  it("an owner resume is not a restart", () => {
    const record = lineage({
      chapters: [
        { instanceId: "a" as Id, generation: 1, createdAt: "2026-10-30T08:00:00Z" },
        { instanceId: "b" as Id, generation: 2, chapterCause: "owner-resume", createdAt: "2026-10-30T09:00:00Z" },
      ],
    });
    expect(restartsIn24h(record, NOW)).toEqual({ count: 0, causes: [] });
  });
});

function message(seq: number, role: "user" | "assistant", text: string, commandId?: string): Observation {
  return {
    seq: String(seq),
    kind: "message",
    instanceId: "ins_main",
    payload: { role, text, ...(commandId ? { commandId } : {}) },
  } as unknown as Observation;
}

function bubble(patch: Partial<LocalBubble>): LocalBubble {
  return {
    clientRequestId: "loc_1" as Id,
    instanceId: "ins_main" as Id,
    text: "看一下 gate",
    commandId: "cmd_1" as Id,
    state: "accepted",
    outboxState: "sent",
    createdAt: "2026-10-30T11:59:00Z",
    ...patch,
  };
}

describe("mainEcho line 2", () => {
  const tailWithReply = readMainTail([message(3, "assistant", "\n第一行回复\n第二行")]);

  it("shows the first line of the last reply", () => {
    expect(firstLine("\n  第一行回复 \n第二行")).toBe("第一行回复");
    const echo = mainEcho({ entry: entry(), bubbles: [], tail: tailWithReply, busy: false, online: true });
    expect(echo).toEqual({ kind: "reply", text: "第一行回复" });
  });

  it("right after a send shows 已发送 · Main 正在处理…, then the reply that follows it in the journal", () => {
    const sent = bubble({});
    const before = mainEcho({ entry: entry(), bubbles: [sent], tail: tailWithReply, busy: true, online: true });
    expect(before).toEqual({ kind: "sending", text: "已发送 · Main 正在处理…" });
    const after = readMainTail([
      message(3, "assistant", "旧回复"),
      message(5, "user", "看一下 gate", "cmd_1"),
      message(7, "assistant", "gate 两项红"),
    ]);
    expect(mainEcho({ entry: entry(), bubbles: [sent], tail: after, busy: false, online: true })).toEqual({
      kind: "reply",
      text: "gate 两项红",
    });
  });

  it("a rejection is red with 重试; a superseded chapter's rejection offers the current Main", () => {
    const rejected = bubble({ outboxState: "rejected", outboxError: "instance exited", state: "unknown" });
    const plain = mainEcho({ entry: entry(), bubbles: [rejected], tail: null, busy: false, online: true });
    expect(plain).toMatchObject({ kind: "rejected", superseded: false, reason: "instance exited" });
    const current = entry({ id: "ins_g2" as Id }, true, 2);
    current.chapterIds = ["ins_main" as Id, "ins_g2" as Id];
    const superseded = mainEcho({ entry: current, bubbles: [rejected], tail: null, busy: false, online: true });
    expect(superseded).toMatchObject({ kind: "rejected", superseded: true });
  });

  it("an unknown outcome asks for a check, held and offline rows say they queue", () => {
    const unknown = bubble({ outboxState: "unknown", state: "unknown" });
    expect(mainEcho({ entry: entry(), bubbles: [unknown], tail: null, busy: false, online: true }).kind).toBe("unknown");
    const held = bubble({ outboxState: "held" });
    expect(mainEcho({ entry: entry(), bubbles: [held], tail: null, busy: false, online: true })).toEqual({
      kind: "queued",
      text: "已排队 · 主机离线，上线后送达",
    });
    const pending = bubble({ outboxState: "pending", state: "queued" });
    expect(mainEcho({ entry: entry(), bubbles: [pending], tail: null, busy: false, online: false }).kind).toBe("queued");
  });

  it("a fresh chapter with no reply of its own keeps the carried reply", () => {
    const fresh = readMainTail([]);
    expect(
      mainEcho({ entry: entry(), bubbles: [], tail: fresh, busy: false, online: true, carriedReply: "上一章的回复" }),
    ).toEqual({ kind: "reply", text: "上一章的回复" });
  });

  it("ignores sends to other sessions", () => {
    const other = bubble({ instanceId: "ins_other" as Id });
    expect(mainEcho({ entry: entry(), bubbles: [other], tail: tailWithReply, busy: false, online: true }).kind).toBe("reply");
  });
});
