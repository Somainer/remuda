import { describe, expect, it } from "vitest";
import { known } from "../../types/wire";
import { inst, interaction, source, task } from "./queueTestData";
import { quickAskChips, stuckCount } from "./quickAsk";

/**
 * ⑨ quick-ask chips (home.md §6.3, task home-main-extras): the four named
 * candidates, the grant mapping, and numbers only when a real count exists.
 */

describe("⑨ quick-ask candidates and the grant mapping", () => {
  it("lists all four for a Main holding address-owner and land", () => {
    const chips = quickAskChips({
      grants: ["address-owner", "land"],
      needYou: 2,
      stuck: 1,
    });
    expect(chips.map((chip) => chip.text)).toEqual([
      "谁在等我",
      "昨晚跑完了什么",
      "哪些卡住了",
      "今天落地了什么",
    ]);
  });

  it("drops 今天落地了什么 for a seat without land", () => {
    const chips = quickAskChips({ grants: ["address-owner", "dispatch"], needYou: 0, stuck: 0 });
    expect(chips.map((chip) => chip.text)).not.toContain("今天落地了什么");
    expect(chips.map((chip) => chip.text)).toContain("谁在等我");
  });

  it("drops 谁在等我 without address-owner (the owner-scope read)", () => {
    const chips = quickAskChips({ grants: ["land"], needYou: 3, stuck: 0 });
    expect(chips.map((chip) => chip.text)).not.toContain("谁在等我");
  });
});

describe("⑨ numbers only from real counts", () => {
  it("paints the ① N on 谁在等我 and the blocked/stalled count on 哪些卡住了", () => {
    const chips = quickAskChips({ grants: ["address-owner"], needYou: 2, stuck: 3 });
    const label = (id: string) => chips.find((chip) => chip.id === id)?.label;
    expect(label("who-waits")).toBe("谁在等我 2");
    expect(label("stuck")).toBe("哪些卡住了 3");
  });

  it("paints no number for a zero or an unknown count (§0.3)", () => {
    const zero = quickAskChips({ grants: ["address-owner"], needYou: 0, stuck: 0 });
    expect(zero.map((chip) => chip.label)).toEqual(["谁在等我", "昨晚跑完了什么", "哪些卡住了"]);
    const unknown = quickAskChips({ grants: ["address-owner"], needYou: null, stuck: null });
    expect(unknown.find((chip) => chip.id === "who-waits")?.label).toBe("谁在等我");
    expect(unknown.find((chip) => chip.id === "stuck")?.label).toBe("哪些卡住了");
  });

  it("never numbers the two questions with no count source", () => {
    const chips = quickAskChips({ grants: ["address-owner", "land"], needYou: 5, stuck: 5 });
    expect(chips.find((chip) => chip.id === "last-night")?.label).toBe("昨晚跑完了什么");
    expect(chips.find((chip) => chip.id === "landed")?.label).toBe("今天落地了什么");
  });
});

describe("⑨ the blocked/stalled count", () => {
  it("counts blocked sessions plus owner-blocked tasks with no waiting session", () => {
    const blockedSession = inst({ id: "ins_blocked", activity: known("waiting-interaction") });
    const idle = inst({ id: "ins_idle" });
    const blockedTask = task({ id: "tsk_blocked", blockedReason: "等你确认验收范围" });
    const sourceWith = source({
      instances: [blockedSession, idle],
      interactions: [interaction({ id: "int_1", instanceId: "ins_blocked" })],
      tasks: [blockedTask],
    });
    expect(stuckCount(sourceWith)).toBe(2);
  });

  it("does not double-count a task whose session already waits on an interaction", () => {
    const blockedSession = inst({ id: "ins_blocked", activity: known("waiting-interaction"), taskId: "tsk_1" });
    const withSession = task({ id: "tsk_1", blockedReason: "等你确认" });
    const sourceWith = source({
      instances: [blockedSession],
      interactions: [interaction({ id: "int_1", instanceId: "ins_blocked" })],
      tasks: [withSession],
    });
    expect(stuckCount(sourceWith)).toBe(1);
  });
});
