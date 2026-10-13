import { describe, expect, it } from "vitest";
import type { UsagePayload } from "../../types/generated";
import { effectiveFromObservation, effectiveFromRecord, effortFlagMismatch, effortLevelMismatch } from "./effortEffective";
import {
  CLAUDE_XHIGH_INDEX,
  contextPercent,
  defaultEffortIndex,
  effortAt,
  effortAtStop,
  effortCaps,
  effortDefaultIndex,
  effortFromRecord,
  effortIndexFromClientX,
  effortLook,
  effortRatio,
  effortStops,
  effortStopIndex,
  effortStopName,
  effortTable,
  effortWireName,
  isEmberEffort,
  isEmberName,
  isEmberTier,
  claudeDefaultTier,
  claudeVersionGate,
  coupledSelection,
  lookupModelEffortRow,
  modelUltracodeCapable,
  normalizeClaudeName,
  normalizeHarnessName,
  nativeEffortWord,
  parseClaudeVersion,
  snapEffortIndex,
  keyboardEffortIndex,
  mapEffort,
  mapEffortIndex,
  UnknownEffortError,
} from "./effort";

describe("harness-native effort tables", () => {
  it("exposes the real Claude levels in CLI order, all five, no sixth ultracode tier", () => {
    expect(effortTable("claude").map((t) => t.name)).toEqual([
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
    ]);
    expect(effortTable("claude").map((t) => t.name)).not.toContain("ultracode");
  });

  it("exposes all six Codex 0.154.0 picker tiers in native order", () => {
    expect(effortTable("codex").map((t) => t.name)).toEqual([
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
      "ultra",
    ]);
  });

  it("exposes the grok menu, now the verified low..xhigh", () => {
    expect(effortTable("grok").map((t) => t.name)).toEqual(["low", "medium", "high", "xhigh"]);
  });

  it("terminal/generic tables are empty for the slider", () => {
    expect(effortTable("terminal")).toEqual([]);
    expect(effortTable("generic")).toEqual([]);
    expect(effortTable("agy")).toEqual([{ name: "default", description: "agy CLI 默认档" }]);
  });
});

describe("default markers (legacy fallback only)", () => {
  it("marks codex/grok medium", () => {
    expect(effortDefaultIndex("codex")).toBe(1);
    expect(defaultEffortIndex("grok")).toBe(1);
  });

  it("falls back to high for claude but that is not a UI default marker (D-056)", () => {
    expect(effortDefaultIndex("claude")).toBe(2);
  });
});

describe("five-stop Claude slider", () => {
  it("lists five tier stops for Claude in order; ultracode is not a stop", () => {
    const stops = effortStops("claude");
    expect(stops.map((s) => s.name)).toEqual(["low", "medium", "high", "xhigh", "max"]);
    expect(stops).toHaveLength(5);
  });

  it("round-trips every stop through stop position and selection", () => {
    const expected = [
      { name: "low", index: 0, ultracode: false },
      { name: "medium", index: 1, ultracode: false },
      { name: "high", index: 2, ultracode: false },
      { name: "xhigh", index: 3, ultracode: false },
      { name: "max", index: 4, ultracode: false },
    ];
    for (const [stop, expect0] of effortStops("claude").map((s, i) => [s, expected[i]] as const)) {
      const selection = effortAtStop("claude", stop.index);
      expect(selection).toMatchObject(expect0);
      expect(effortStopIndex("claude", selection.index)).toBe(stop.index);
      expect(effortStopName("claude", selection.name)).toBe(stop.name ?? stop.label ?? stop.name);
    }
  });

  it("carries the orthogonal flag independently of the tier", () => {
    const onMax = effortAt("claude", 4, true);
    expect(onMax.name).toBe("max");
    expect(onMax.index).toBe(4);
    expect(onMax.ultracode).toBe(true);
    const onMedium = effortAt("claude", 1, true);
    expect(onMedium).toMatchObject({ name: "medium", index: 1, ultracode: true });
    const off = effortAt("claude", 3, false);
    expect(off).toMatchObject({ name: "xhigh", index: 3, ultracode: false });
  });

  it("steps with arrows across five stops", () => {
    expect(keyboardEffortIndex(0, "ArrowRight", 5)).toBe(1);
    expect(keyboardEffortIndex(3, "ArrowRight", 5)).toBe(4);
    expect(keyboardEffortIndex(4, "ArrowRight", 5)).toBe(4);
    expect(keyboardEffortIndex(0, "ArrowLeft", 5)).toBe(0);
    expect(keyboardEffortIndex(4, "End", 5)).toBe(4);
    expect(keyboardEffortIndex(4, "Home", 5)).toBe(0);
  });
});

describe("ultracode version gate", () => {
  it("parses semver-ish binary versions", () => {
    expect(parseClaudeVersion("2.1.289 (Claude Code)")).toEqual([2, 1, 289]);
    expect(parseClaudeVersion("2.1.272")).toEqual([2, 1, 272]);
    expect(parseClaudeVersion(null)).toBeNull();
    expect(parseClaudeVersion("abc")).toBeNull();
  });

  it("classifies decoupled / coupled / legacy / unknown", () => {
    expect(claudeVersionGate("2.1.289")).toBe("decoupled");
    expect(claudeVersionGate("2.1.284")).toBe("decoupled");
    expect(claudeVersionGate("2.1.277")).toBe("coupled");
    expect(claudeVersionGate("2.1.203")).toBe("coupled");
    expect(claudeVersionGate("2.1.202")).toBe("legacy");
    expect(claudeVersionGate(null)).toBe("unknown");
    expect(claudeVersionGate("")).toBe("unknown");
  });

  it("coupledSelection forces xhigh; decoupled effortAt never does", () => {
    const onMax = effortAt("claude", 4, true);
    expect(onMax.name).toBe("max");
    expect(coupledSelection(onMax)).toMatchObject({ name: "xhigh", index: CLAUDE_XHIGH_INDEX, ultracode: true });
    const onMedium = effortAt("claude", 1, true);
    expect(coupledSelection(onMedium).index).toBe(CLAUDE_XHIGH_INDEX);
    // No flag: nothing happens.
    expect(coupledSelection(effortAt("claude", 0, false)).name).toBe("low");
  });
});

describe("legacy name normalization", () => {
  it("maps Claude names by name to the new levels", () => {
    expect(normalizeClaudeName("default")).toMatchObject({ tier: "low", index: 0, ultracode: false });
    expect(normalizeClaudeName("think")).toMatchObject({ tier: "high" });
    expect(normalizeClaudeName("think-hard")).toMatchObject({ tier: "xhigh" });
  });

  it("legacy stored ultracode prefs/drafts/names load as {xhigh, on}", () => {
    expect(normalizeClaudeName("ultracode")).toMatchObject({ tier: "xhigh", index: 3, ultracode: true });
    expect(effortFromRecord("claude", "ultracode")).toMatchObject({
      name: "xhigh",
      index: 3,
      ultracode: true,
    });
  });

  it("preserves {xhigh, flag false} explicitly while reading current names", () => {
    expect(effortFromRecord("claude", "xhigh", null, false)).toMatchObject({
      name: "xhigh",
      ultracode: false,
    });
  });

  it("unknown words land on the legacy fallback high", () => {
    expect(normalizeClaudeName("bogus")).toMatchObject({ tier: "high", index: 2, ultracode: false });
  });

  it("migrates codex minimal to low; unknowns to the default; max/ultra stay current", () => {
    expect(normalizeHarnessName("codex", "minimal")).toEqual({ tier: "low", index: 0 });
    expect(normalizeHarnessName("codex", "bogus")).toEqual({ tier: "medium", index: 1 });
    expect(normalizeHarnessName("codex", "ultra")).toEqual({ tier: "ultra", index: 5 });
  });

  it("migrates the invented grok quick/standard/max names", () => {
    expect(normalizeHarnessName("grok", "standard")).toEqual({ tier: "medium", index: 1 });
    expect(normalizeHarnessName("grok", "quick")).toEqual({ tier: "low", index: 0 });
    expect(normalizeHarnessName("grok", "max")).toEqual({ tier: "xhigh", index: 3 });
  });
});

describe("native wire vocabulary is closed", () => {
  it("returns current words; throws on anything else", () => {
    expect(nativeEffortWord("codex", "max")).toBe("max");
    expect(() => nativeEffortWord("codex", "ultracode")).toThrow(UnknownEffortError);
    expect(() => nativeEffortWord("codex", "bogus")).toThrow(UnknownEffortError);
  });

  it("effortWireName emits only native tier words — never the legacy ultracode name", () => {
    expect(effortWireName(effortAt("claude", 3, true))).toBe("xhigh");
    expect(effortWireName(effortAt("claude", 4, true))).toBe("max");
    const codex = effortFromRecord("codex", "ultra", 0, false);
    expect(effortWireName(codex!)).toBe("ultra");
  });
});

describe("visual ladder", () => {
  it("Claude plain tiers", () => {
    for (const i of [0, 1, 2]) expect(effortLook("claude", i, false)).toBe("plain");
  });

  it("Claude xhigh/max are static top, never ember without the flag", () => {
    expect(effortLook("claude", CLAUDE_XHIGH_INDEX, false)).toBe("top");
    expect(effortLook("claude", 4, false)).toBe("top");
  });

  it("ember follows the orthogonal SWITCH at any tier, not the slider", () => {
    expect(effortLook("claude", 4, true)).toBe("ultracode");
    expect(effortLook("claude", 2, true)).toBe("ultracode");
    expect(effortLook("claude", 1, true)).toBe("ultracode");
    expect(isEmberEffort("claude", 2, true)).toBe(true);
    expect(isEmberEffort("claude", 4, false)).toBe(false);
    expect(isEmberEffort("claude", 3, false)).toBe(false);
  });

  it("Codex ember is its native Ultra tier; flag never sets it", () => {
    expect(effortLook("codex", 5, false)).toBe("ultracode");
    expect(effortLook("codex", 4, false)).toBe("top");
  });

  it("grok top is static accent, no ember", () => {
    expect(effortLook("grok", 3, false)).toBe("top");
  });

  it("isEmberTier marks only the last native tier (used by codex lists)", () => {
    expect(isEmberTier("claude", 4)).toBe(true);
    expect(isEmberTier("codex", 5)).toBe(true);
    expect(isEmberTier("agy", 0)).toBe(false);
  });
});

describe("mapping by index after harness change", () => {
  it("maps max Claude to max Codex on a ratio basis", () => {
    expect(mapEffortIndex(4, 5, 6)).toBe(5);
    expect(mapEffortIndex(0, 5, 4)).toBe(0);
  });

  it("keeps the FLAG inside Claude at the mapped tier; it cannot return via Codex (D-056)", () => {
    const onMax = effortAt("claude", 4, true);
    // Leaving Claude drops the flag entirely (Codex has no such axis).
    const codex = mapEffort(onMax, "codex");
    expect(codex.ultracode).toBeUndefined();
    // A Claude→Claude remap keeps the flag at its tier, never forced to xhigh.
    const low = effortAt("claude", 0, false);
    void low;
    expect(mapEffort(onMax, "claude")).toMatchObject({ index: 4, ultracode: true });
  });

  it("drops the flag leaving Claude and never enters elsewhere", () => {
    const onMax = effortAt("claude", 3, true);
    expect(mapEffort(onMax, "codex").ultracode).toBeUndefined();
    mapEffort(effortAt("codex", 4, false), "grok");
  });
});

describe("snap/point geometry", () => {
  it("snaps 0..1 ratio onto native tier indices", () => {
    expect(snapEffortIndex(0, 5)).toBe(0);
    expect(snapEffortIndex(0.25, 5)).toBe(1);
    expect(snapEffortIndex(0.75, 5)).toBe(3);
    expect(snapEffortIndex(1, 5)).toBe(4);
    expect(snapEffortIndex(-2, 5)).toBe(0);
    expect(snapEffortIndex(8, 5)).toBe(4);
    expect(snapEffortIndex(Number.NaN, 5)).toBe(0);
  });

  it("maps clientX onto the nearest native stop at 5 stops", () => {
    const track = { left: 100, width: 400 };
    expect(effortIndexFromClientX(100, track, 5)).toBe(0);
    expect(effortIndexFromClientX(500, track, 5)).toBe(4);
    expect(effortIndexFromClientX(300, track, 5)).toBe(2);
    expect(effortIndexFromClientX(0, { left: 0, width: 0 }, 5)).toBe(0);
  });

  it("a single tier and empty table edge cases", () => {
    expect(effortRatio(0, 1)).toBe(1);
    expect(effortRatio(0, 0)).toBe(0);
    snapEffortIndex(0.5, 1);
  });
});

describe("per-model default effort (D-056 §6)", () => {
  it("marks Opus/Sonnet 5.x medium and the rest high; unknown model null", () => {
    expect(claudeDefaultTier("opus")?.name).toBe("medium");
    expect(claudeDefaultTier("claude-opus-5")?.index).toBe(1);
    expect(claudeDefaultTier("sonnet")).toEqual({ name: "medium", index: 1 });
    expect(claudeDefaultTier("fable")?.name).toBe("high");
    expect(claudeDefaultTier("haiku")?.name).toBe("high");
    expect(claudeDefaultTier("gw/mystery-model")).toBeNull();
    expect(claudeDefaultTier(null)).toBeNull();
  });

  it("matches gateway family- and dated-snapshot spellings", () => {
    expect(claudeDefaultTier("acme/claude-opus-5-20251001")?.name).toBe("medium");
    expect(claudeDefaultTier("claude-haiku-4-5[1m]")?.name).toBe("high");
  });

  it("prefers explicit Hub catalog rows when supplied", () => {
    const rows = [
      { id: "claude-opus-5", aliases: ["opus"], defaultEffort: "high", ultracodeCapable: false },
    ];
    expect(claudeDefaultTier("opus", rows)).toEqual({ name: "high", index: 2 });
    expect(modelUltracodeCapable("opus", rows)).toBe(false);
  });

  it("falls back to built-in rows when catalog is empty", () => {
    expect(claudeDefaultTier("opus", [])?.name).toBe("medium");
    expect(modelUltracodeCapable("opus", [])).toBe(true);
    expect(lookupModelEffortRow("opus", null)?.id).toBe("claude-opus-5");
  });

  it("resolves Opus 4.7 canonical, alias, dated and context spellings (default xhigh)", () => {
    for (const id of ["claude-opus-4-7", "opus-4-7", "claude-opus-4-7-20250824", "claude-opus-4-7[1m]"]) {
      expect(claudeDefaultTier(id, [])?.index).toBe(3);
      expect(claudeDefaultTier(id, [])?.name).toBe("xhigh");
      expect(modelUltracodeCapable(id, [])).toBe(true);
      expect(lookupModelEffortRow(id, null)?.id).toBe("claude-opus-4-7");
    }
    // Bare "opus" still means the current Opus 5 (longest/alias, not 4.7).
    expect(lookupModelEffortRow("opus", null)?.id).toBe("claude-opus-5");
  });
});

describe("effortEffective per-axis mismatches", () => {
  const eff = (name: string, ultracode: boolean | null, source = "slash") => ({
    kind: "effort",
    payload: {
      effective: { name, ultracode, source, observedAt: "2026-09-16T00:00:01.000Z" },
    },
  });

  it("level mismatch ignores the flag", () => {
    expect(effortLevelMismatch("max", (effectiveFromObservation(eff("high", null))?.effective ?? null))).toEqual({
      requested: "max",
      effective: "high",
    });
    // Same name with flag differing: level axis agrees.
    expect(effortLevelMismatch("max", effectiveFromObservation(eff("max", true))?.effective ?? null)).toBeNull();
  });

  it("flag mismatch is its own axis, including unobserved", () => {
    const unknownFlag = effectiveFromObservation(eff("max", null))?.effective ?? null;
    expect(effortFlagMismatch(true, unknownFlag)).toEqual({ requested: "on", observed: "unknown" });
    expect(effortFlagMismatch(true, effectiveFromObservation(eff("max", false))?.effective ?? null)).toEqual({
      requested: "on",
      observed: "off",
    });
    expect(effortFlagMismatch(true, effectiveFromObservation(eff("max", true))?.effective ?? null)).toBeNull();
    expect(effortFlagMismatch(false, effectiveFromObservation(eff("max", true))?.effective ?? null)).toEqual({
      requested: "off",
      observed: "on",
    });
    // Off + unreported flag is agreement, never a mismatch.
    expect(effortFlagMismatch(false, unknownFlag)).toBeNull();
  });
});

describe("context percent", () => {
  it("uses known tokens for the window, null otherwise", () => {
    const payload = {
      totalTokens: { state: "unknown" as const, reason: "x", evidenceEventIds: [] },
    } as unknown as UsagePayload;
    expect(contextPercent(undefined, "claude")).toBeNull();
    expect(contextPercent(payload, "claude")).toBeNull();
    (payload as { totalTokens: unknown }).totalTokens = { state: "known", value: "148000" };
    expect(contextPercent(payload, "claude")).toBe(74);
  });
});

describe("caps and names", () => {
  it("effortCaps matrix", () => {
    expect(effortCaps("terminal").effort).toBe(false);
    expect(effortCaps("agy").model).toBe(false);
    expect(effortCaps("claude")).toMatchObject({ model: true, effort: true });
  });

  it("isEmberName is true only for a native ember tier (Codex ultra)", () => {
    expect(isEmberName("codex", "ultra")).toBe(true);
    expect(isEmberName("claude", "max")).toBe(false);
  });
});

describe("read-back-unavailable edge (D-056 (4))", () => {
  const withdrawn = {
    kind: "effort" as const,
    payload: {
      requested: { name: "max", ultracode: false },
      effective: {
        name: null,
        ultracode: null,
        source: "unknown",
        observedAt: "2026-10-08T12:05:00Z",
        readbackAvailable: false,
      },
    },
  };

  it("withdraws the projected record: effectiveFromRecord returns null", () => {
    expect(effectiveFromRecord(withdrawn.payload.effective)).toBeNull();
  });

  it("marks the observation withdrawn with a null effective view", () => {
    const parsed = effectiveFromObservation(withdrawn)!;
    expect(parsed.withdrawn).toBe(true);
    expect(parsed.effective).toBeNull();
    expect(parsed.requested?.name).toBe("max");
  });

  it("a normal edge is never marked withdrawn", () => {
    const parsed = effectiveFromObservation({
      kind: "effort",
      payload: {
        effective: { name: "high", source: "slash", observedAt: "2026-10-08T12:00:00Z" },
      },
    })!;
    expect(parsed.withdrawn).toBe(false);
    expect(parsed.effective?.name).toBe("high");
  });

  it("a non-effort event returns nothing", () => {
    expect(
      effectiveFromObservation({ kind: "lifecycle", payload: { type: "native" } }),
    ).toBeNull();
  });
});
