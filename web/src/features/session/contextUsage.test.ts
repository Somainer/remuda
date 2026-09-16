import { describe, expect, it } from "vitest";
import {
  coerceUsageRollup,
  contextHeadline,
  exactTokenCount,
  formatTokenCount,
  lastTurnLabel,
  sessionCells,
  tpmCells,
  type UsageRollup,
} from "./contextUsage";

describe("coerceUsageRollup", () => {
  it("accepts the full wire shape", () => {
    const rollup = coerceUsageRollup({
      contextUsedTokens: 1000,
      contextWindowTokens: 200000,
      contextPct: 1,
      sessionInputTokens: 10,
      sessionOutputTokens: 20,
      cacheReadTokens: 990,
      cacheCreationTokens: 0,
      turns: 2,
      tpmIn60s: 10,
      tpmOut60s: 20,
      tpmIn5m: 2,
      tpmOut5m: 4,
      lastTurnAt: "2026-09-17T00:00:00.000Z",
    });
    expect(rollup?.turns).toBe(2);
    expect(rollup?.contextUsedTokens).toBe(1000);
    expect(rollup?.lastTurnAt).toBe("2026-09-17T00:00:00.000Z");
  });

  it("treats missing, string, and negative fields as unknown, never zero", () => {
    const rollup = coerceUsageRollup({
      turns: "1",
      contextPct: "18%",
      sessionInputTokens: -5,
      tpmIn60s: undefined,
      lastTurnAt: 42,
    });
    expect(rollup).not.toBeNull();
    expect(rollup?.turns).toBe(0);
    expect(rollup?.contextPct).toBeNull();
    expect(rollup?.sessionInputTokens).toBeNull();
    expect(rollup?.tpmIn60s).toBeNull();
    expect(rollup?.lastTurnAt).toBeNull();
    expect(rollup?.contextWindowTokens).toBeNull();
  });

  it("returns null for non-object payloads", () => {
    expect(coerceUsageRollup(null)).toBeNull();
    expect(coerceUsageRollup(undefined)).toBeNull();
    expect(coerceUsageRollup("nope")).toBeNull();
  });
});

describe("formatTokenCount", () => {
  it("uses exact small ints, one-decimal k and two-decimal M", () => {
    expect(formatTokenCount(null)).toBe("—");
    expect(formatTokenCount(0)).toBe("0");
    expect(formatTokenCount(589)).toBe("589");
    expect(formatTokenCount(35_839)).toBe("35.8k");
    expect(formatTokenCount(97_704)).toBe("97.7k");
    expect(formatTokenCount(200_000)).toBe("200.0k");
    expect(formatTokenCount(1_000_000)).toBe("1.00M");
  });

  it("exact counts are group-separated for tooltips", () => {
    expect(exactTokenCount(97_704)).toBe("97,704");
    expect(exactTokenCount(null)).toBe("");
  });
});

describe("contextHeadline", () => {
  it("renders used/window and the percentage when both known", () => {
    const head = contextHeadline({
      contextUsedTokens: 35_839,
      contextWindowTokens: 200_000,
      contextPct: 18,
    } as UsageRollup);
    expect(head.text).toBe("35.8k/200.0k (18%)");
    expect(head.missing).toBeNull();
  });

  it("points at the usage channel when the used figure is unknown", () => {
    const head = contextHeadline({
      contextUsedTokens: null,
      contextWindowTokens: 200_000,
      contextPct: null,
    } as UsageRollup);
    expect(head.text).toBe("—/200.0k (—%)");
    expect(head.missing).toMatch(/usage 观察/);
  });
});

describe("sessionCells / tpmCells / lastTurnLabel", () => {
  const rollup = coerceUsageRollup({
    contextUsedTokens: null,
    contextWindowTokens: 128_000,
    contextPct: null,
    sessionInputTokens: null,
    sessionOutputTokens: 420,
    cacheReadTokens: null,
    cacheCreationTokens: null,
    turns: 1,
    tpmIn60s: null,
    tpmOut60s: 420,
    tpmIn5m: null,
    tpmOut5m: 84,
    lastTurnAt: null,
  })!;

  it("marks each unknown cell with its missing channel", () => {
    const cells = sessionCells(rollup);
    expect(cells.map((c) => c.label)).toEqual(["入", "出", "缓存读", "缓存写"]);
    expect(cells[0].value).toBe("—");
    expect(cells[0].missingChannel).toMatch(/inputTokens/);
    expect(cells[1].value).toBe("420");
    expect(cells[1].missingChannel).toBeNull();
  });

  it("keeps the 60 s and 5 min TPM rows distinct and labeled", () => {
    const rows = tpmCells(rollup);
    expect(rows.map((r) => r.label)).toEqual(["60 秒", "5 分钟"]);
    expect(rows[0].in.value).toBe("—");
    expect(rows[0].in.missingChannel).not.toBeNull();
    expect(rows[0].out.value).toBe("420");
    expect(rows[1].out.value).toBe("84");
    expect(rows[1].in.missingChannel).not.toBeNull();
  });

  it("lastTurnLabel renders the dash and channel without a timestamp", () => {
    const cell = lastTurnLabel(rollup, Date.now());
    expect(cell.value).toBe("—");
    expect(cell.missingChannel).toMatch(/usage 观察/);
  });
});
