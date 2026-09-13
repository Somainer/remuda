import { describe, expect, it } from "vitest";
import {
  contextPercent,
  DEFAULT_EFFORT_INDEX,
  defaultEffortIndex,
  effortAt,
  effortIndexFromClientX,
  effortRatio,
  effortTable,
  isEmberTier,
  keyboardEffortIndex,
  mapEffort,
  mapEffortIndex,
  shortModel,
  snapEffortIndex,
} from "./effort";
import type { UsagePayload } from "../../types/generated";
import { known, unknownKnowledge } from "../../types/wire";

describe("harness-native effort tables", () => {
  it("exposes claude / codex / grok native names, never fast/standard/deep", () => {
    expect(effortTable("claude").map((t) => t.name)).toEqual(["default", "think", "think-hard", "ultracode"]);
    expect(effortTable("codex").map((t) => t.name)).toEqual(["low", "medium", "high", "ultra"]);
    expect(effortTable("grok").map((t) => t.name)).toEqual(["quick", "standard", "max"]);
    expect(effortTable("agy").map((t) => t.name)).toEqual(["default"]);
    expect(effortTable("terminal")).toEqual([]);
    const names = [...effortTable("claude"), ...effortTable("codex"), ...effortTable("grok")].map((t) => t.name);
    expect(names).not.toContain("fast");
    expect(names).not.toContain("deep");
  });

  it("marks the last row as the ember tier", () => {
    expect(isEmberTier("claude", 3)).toBe(true);
    expect(isEmberTier("claude", 1)).toBe(false);
    expect(isEmberTier("codex", 3)).toBe(true);
    expect(isEmberTier("grok", 2)).toBe(true);
    expect(isEmberTier("agy", 0)).toBe(true);
  });

  it("maps by index so the top tier stays the top tier", () => {
    expect(mapEffortIndex(3, 4, 3)).toBe(2);
    expect(mapEffortIndex(0, 4, 3)).toBe(0);
    expect(mapEffortIndex(1, 4, 3)).toBe(1);
    const ultra = effortAt("claude", 3);
    expect(mapEffort(ultra, "grok")).toEqual({ index: 2, name: "max", kind: "grok" });
    expect(mapEffort(ultra, "codex")).toEqual({ index: 3, name: "ultra", kind: "codex" });
    expect(mapEffort(effortAt("claude", DEFAULT_EFFORT_INDEX), "codex").name).toBe("medium");
    expect(mapEffort(effortAt("grok", 2), "claude").name).toBe("ultracode");
  });

  it("clamps a settings default onto a shorter table", () => {
    expect(effortAt("agy", DEFAULT_EFFORT_INDEX).name).toBe("default");
    expect(effortAt("grok", 9).name).toBe("max");
  });

  it("shortens model ids for the chip", () => {
    expect(shortModel("passthrough/auto")).toBe("auto");
    expect(shortModel("opus")).toBe("opus");
  });
});

describe("effort slider snapping", () => {
  it("snaps a 0..1 ratio onto native claude / grok / single-tier tables", () => {
    expect(snapEffortIndex(0, 4)).toBe(0);
    expect(snapEffortIndex(0.2, 4)).toBe(1);
    expect(snapEffortIndex(0.5, 4)).toBe(2);
    expect(snapEffortIndex(0.9, 4)).toBe(3);
    expect(snapEffortIndex(1, 4)).toBe(3);
    expect(snapEffortIndex(-2, 4)).toBe(0);
    expect(snapEffortIndex(8, 3)).toBe(2);
    expect(snapEffortIndex(0.4, 3)).toBe(1);
    expect(snapEffortIndex(0.7, 1)).toBe(0);
    expect(snapEffortIndex(Number.NaN, 4)).toBe(0);
  });

  it("maps a pointer x onto the nearest stop", () => {
    const track = { left: 100, width: 300 };
    expect(effortIndexFromClientX(100, track, 4)).toBe(0);
    expect(effortIndexFromClientX(200, track, 4)).toBe(1);
    expect(effortIndexFromClientX(250, track, 4)).toBe(2);
    expect(effortIndexFromClientX(400, track, 4)).toBe(3);
    expect(effortIndexFromClientX(0, { left: 0, width: 0 }, 4)).toBe(0);
  });

  it("places the knob at 0/⅓/⅔/1 and parks a single tier at the ember end", () => {
    expect(effortRatio(0, 4)).toBe(0);
    expect(effortRatio(1, 4)).toBeCloseTo(1 / 3);
    expect(effortRatio(3, 4)).toBe(1);
    expect(effortRatio(0, 1)).toBe(1);
    expect(effortRatio(0, 0)).toBe(0);
  });

  it("steps with arrows and jumps with Home/End", () => {
    expect(keyboardEffortIndex(1, "ArrowRight", 4)).toBe(2);
    expect(keyboardEffortIndex(1, "ArrowLeft", 4)).toBe(0);
    expect(keyboardEffortIndex(0, "ArrowLeft", 4)).toBe(0);
    expect(keyboardEffortIndex(3, "ArrowRight", 4)).toBe(3);
    expect(keyboardEffortIndex(1, "ArrowUp", 4)).toBe(2);
    expect(keyboardEffortIndex(1, "ArrowDown", 4)).toBe(0);
    expect(keyboardEffortIndex(2, "Home", 4)).toBe(0);
    expect(keyboardEffortIndex(0, "End", 4)).toBe(3);
    expect(keyboardEffortIndex(1, "End", 3)).toBe(2);
    expect(keyboardEffortIndex(1, "Enter", 4)).toBeNull();
    expect(defaultEffortIndex("claude")).toBe(1);
    expect(defaultEffortIndex("agy")).toBe(0);
  });
});

describe("context percent", () => {
  it("uses known tokens against the harness window", () => {
    const payload = {
      inputTokens: known("148000"),
      outputTokens: known("0"),
      totalTokens: unknownKnowledge("none"),
    } as UsagePayload;
    expect(contextPercent(payload, "claude")).toBe(74);
  });

  it("hides the number when tokens are unknown", () => {
    const payload = {
      inputTokens: unknownKnowledge("none"),
      outputTokens: unknownKnowledge("none"),
      totalTokens: unknownKnowledge("none"),
    } as UsagePayload;
    expect(contextPercent(payload, "claude")).toBeNull();
  });
});
