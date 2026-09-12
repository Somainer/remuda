import { describe, expect, it } from "vitest";
import {
  contextPercent,
  DEFAULT_EFFORT_INDEX,
  effortAt,
  effortTable,
  isEmberTier,
  mapEffort,
  mapEffortIndex,
  shortModel,
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
