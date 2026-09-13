import { describe, expect, it } from "vitest";
import {
  CLAUDE_ULTRACODE_INDEX,
  contextPercent,
  DEFAULT_EFFORT_INDEX,
  defaultEffortIndex,
  effortAt,
  effortFromRecord,
  effortIndexFromClientX,
  effortRatio,
  effortTable,
  effortWireName,
  isEmberEffort,
  isEmberTier,
  keyboardEffortIndex,
  mapEffort,
  mapEffortIndex,
  normalizeClaudeName,
  shortModel,
  snapEffortIndex,
} from "./effort";
import type { UsagePayload } from "../../types/generated";
import { known, unknownKnowledge } from "../../types/wire";

describe("harness-native effort tables", () => {
  it("exposes the real Claude Code levels in CLI order, never fast/standard/deep", () => {
    expect(effortTable("claude").map((t) => t.name)).toEqual([
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
    ]);
    expect(effortTable("codex").map((t) => t.name)).toEqual(["low", "medium", "high", "ultra"]);
    expect(effortTable("grok").map((t) => t.name)).toEqual(["quick", "standard", "max"]);
    expect(effortTable("agy").map((t) => t.name)).toEqual(["default"]);
    expect(effortTable("terminal")).toEqual([]);
    const names = [...effortTable("claude"), ...effortTable("codex"), ...effortTable("grok")].map(
      (t) => t.name,
    );
    expect(names).not.toContain("fast");
    expect(names).not.toContain("deep");
    // The old tier names are gone from the live table.
    expect(effortTable("claude").map((t) => t.name)).not.toContain("ultracode");
    expect(effortTable("claude").map((t) => t.name)).not.toContain("think");
  });

  it("defaults to the Claude high tier (index 2), the CLI default", () => {
    expect(DEFAULT_EFFORT_INDEX).toBe(2);
    expect(defaultEffortIndex("claude")).toBe(2);
    expect(effortAt("claude", DEFAULT_EFFORT_INDEX).name).toBe("high");
    expect(defaultEffortIndex("agy")).toBe(0);
  });

  it("marks only the last row as the ember tier", () => {
    expect(isEmberTier("claude", 4)).toBe(true);
    expect(isEmberTier("claude", 3)).toBe(false);
    expect(isEmberTier("codex", 3)).toBe(true);
    expect(isEmberTier("grok", 2)).toBe(true);
    expect(isEmberTier("agy", 0)).toBe(true);
  });
});

describe("legacy name normalization", () => {
  it("maps legacy Claude names by name onto the new levels", () => {
    expect(normalizeClaudeName("default")).toMatchObject({ tier: "low", index: 0, ultracode: false });
    expect(normalizeClaudeName("think")).toMatchObject({ tier: "high", index: 2, ultracode: false });
    expect(normalizeClaudeName("think-hard")).toMatchObject({ tier: "xhigh", index: 3, ultracode: false });
    // The old ultracode tier becomes the xhigh tier plus the ultracode boolean.
    expect(normalizeClaudeName("ultracode")).toMatchObject({ tier: "xhigh", index: 3, ultracode: true });
    // Current names resolve directly with the boolean off.
    expect(normalizeClaudeName("xhigh")).toMatchObject({ tier: "xhigh", index: 3, ultracode: false });
    expect(normalizeClaudeName("max")).toMatchObject({ tier: "max", index: 4, ultracode: false });
    // Unknown stored values land on the default high.
    expect(normalizeClaudeName("bogus")).toMatchObject({ tier: "high", index: 2, ultracode: false });
  });

  it("rebuilds a selection from a legacy record name", () => {
    expect(effortFromRecord("claude", "think")).toMatchObject({
      name: "high",
      index: 2,
      ultracode: false,
    });
    expect(effortFromRecord("claude", "think-hard")).toMatchObject({ name: "xhigh", index: 3 });
    expect(effortFromRecord("claude", "ultracode")).toMatchObject({
      name: "xhigh",
      index: 3,
      ultracode: true,
    });
    expect(effortFromRecord("claude", "default")).toMatchObject({ name: "low", index: 0 });
    // A current name resolves directly.
    expect(effortFromRecord("claude", "max")).toMatchObject({ name: "max", index: 4 });
    // No name but an index falls back to that stop.
    expect(effortFromRecord("claude", null, 1)).toMatchObject({ name: "medium", index: 1 });
    // Nothing at all yields undefined.
    expect(effortFromRecord("claude")).toBeUndefined();
    // Non-Claude records keep their native name mapping.
    expect(effortFromRecord("grok", "standard")).toMatchObject({ name: "standard", index: 1 });
  });
});

describe("ultracode", () => {
  it("is a boolean that forces the xhigh tier, never a tier itself", () => {
    const on = effortAt("claude", 4, true);
    expect(on.name).toBe("xhigh");
    expect(on.index).toBe(CLAUDE_ULTRACODE_INDEX);
    expect(on.ultracode).toBe(true);
    // Even when asked for another index, ultracode forces xhigh.
    expect(effortAt("claude", 0, true).index).toBe(3);
    // Off leaves the index alone and the boolean explicitly false.
    expect(effortAt("claude", 1, false)).toMatchObject({ index: 1, name: "medium", ultracode: false });
  });

  it("is Claude-only: other harnesses never carry the boolean", () => {
    expect(effortAt("codex", 3, true).ultracode).toBeUndefined();
    expect(effortAt("grok", 2, true).ultracode).toBeUndefined();
    expect(effortAt("agy", 0, true).ultracode).toBeUndefined();
  });

  it("plays ember on max and on ultracode, but not plain xhigh", () => {
    expect(isEmberEffort("claude", 4, false)).toBe(true);
    expect(isEmberEffort("claude", 3, true)).toBe(true);
    expect(isEmberEffort("claude", 3, false)).toBe(false);
    expect(isEmberEffort("claude", 2, false)).toBe(false);
    // Non-Claude harnesses ignore the flag.
    expect(isEmberEffort("codex", 1, true)).toBe(false);
    expect(isEmberEffort("codex", 3, false)).toBe(true);
  });

  it("round-trips on the wire as the legacy ultracode name until x-p1-proto", () => {
    expect(effortWireName(effortAt("claude", 3, true))).toBe("ultracode");
    expect(effortWireName(effortAt("claude", 3, false))).toBe("xhigh");
    expect(effortWireName(effortAt("claude", 4, false))).toBe("max");
    expect(effortWireName(effortAt("codex", 3, false))).toBe("ultra");
  });

  it("drops ultracode when mapping away from Claude and re-locks on return", () => {
    const ultra = effortAt("claude", 3, true);
    const grok = mapEffort(ultra, "grok");
    expect(grok).toMatchObject({ index: 2, name: "max", kind: "grok" });
    expect(grok.ultracode).toBeUndefined();
    // A non-ultra claude selection maps by ratio like any other table.
    const back = mapEffort(grok, "claude");
    expect(back.ultracode).toBe(false);
    // An ultra Claude selection mapped to Claude stays locked on xhigh+ultra.
    expect(mapEffort(ultra, "claude")).toMatchObject({ name: "xhigh", ultracode: true });
  });
});

describe("mapping by index", () => {
  it("maps by index so the top tier stays the top tier", () => {
    expect(mapEffortIndex(4, 5, 3)).toBe(2);
    expect(mapEffortIndex(0, 5, 3)).toBe(0);
    const max = effortAt("claude", 4);
    expect(mapEffort(max, "grok")).toEqual({ index: 2, name: "max", kind: "grok" });
    expect(mapEffort(max, "codex")).toMatchObject({ index: 3, name: "ultra", kind: "codex" });
    expect(mapEffort(effortAt("claude", DEFAULT_EFFORT_INDEX), "codex").name).toBe("high");
  });

  it("clamps an out-of-range index onto a shorter table", () => {
    expect(effortAt("agy", DEFAULT_EFFORT_INDEX).name).toBe("default");
    expect(effortAt("grok", 9).name).toBe("max");
  });

  it("shortens model ids for the chip", () => {
    expect(shortModel("passthrough/auto")).toBe("auto");
    expect(shortModel("opus")).toBe("opus");
  });
});

describe("effort slider snapping", () => {
  it("snaps a 0..1 ratio onto native five / four / single-tier tables", () => {
    expect(snapEffortIndex(0, 5)).toBe(0);
    expect(snapEffortIndex(0.25, 5)).toBe(1);
    expect(snapEffortIndex(0.5, 5)).toBe(2);
    expect(snapEffortIndex(0.75, 5)).toBe(3);
    expect(snapEffortIndex(1, 5)).toBe(4);
    expect(snapEffortIndex(-2, 5)).toBe(0);
    expect(snapEffortIndex(8, 3)).toBe(2);
    expect(snapEffortIndex(0.4, 3)).toBe(1);
    expect(snapEffortIndex(0.7, 1)).toBe(0);
    expect(snapEffortIndex(Number.NaN, 5)).toBe(0);
  });

  it("maps a pointer x onto the nearest stop", () => {
    const track = { left: 100, width: 400 };
    expect(effortIndexFromClientX(100, track, 5)).toBe(0);
    expect(effortIndexFromClientX(200, track, 5)).toBe(1);
    expect(effortIndexFromClientX(300, track, 5)).toBe(2);
    expect(effortIndexFromClientX(400, track, 5)).toBe(3);
    expect(effortIndexFromClientX(500, track, 5)).toBe(4);
    expect(effortIndexFromClientX(0, { left: 0, width: 0 }, 5)).toBe(0);
  });

  it("places the knob at 0/¼/½/¾/1 and parks a single tier at the ember end", () => {
    expect(effortRatio(0, 5)).toBe(0);
    expect(effortRatio(1, 5)).toBeCloseTo(0.25);
    expect(effortRatio(4, 5)).toBe(1);
    expect(effortRatio(0, 1)).toBe(1);
    expect(effortRatio(0, 0)).toBe(0);
  });

  it("steps with arrows and jumps with Home/End across five stops", () => {
    expect(keyboardEffortIndex(1, "ArrowRight", 5)).toBe(2);
    expect(keyboardEffortIndex(1, "ArrowLeft", 5)).toBe(0);
    expect(keyboardEffortIndex(0, "ArrowLeft", 5)).toBe(0);
    expect(keyboardEffortIndex(4, "ArrowRight", 5)).toBe(4);
    expect(keyboardEffortIndex(1, "ArrowUp", 5)).toBe(2);
    expect(keyboardEffortIndex(1, "ArrowDown", 5)).toBe(0);
    expect(keyboardEffortIndex(2, "Home", 5)).toBe(0);
    expect(keyboardEffortIndex(0, "End", 5)).toBe(4);
    expect(keyboardEffortIndex(1, "End", 3)).toBe(2);
    expect(keyboardEffortIndex(1, "Enter", 5)).toBeNull();
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
