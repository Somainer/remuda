import { describe, expect, it } from "vitest";
import {
  parseSkippedEntries,
  skippedSidecarEntries,
  skippedSidecarNotices,
} from "./skippedSidecars";

function diag(over: Record<string, unknown> = {}, kind = "lifecycle"): unknown {
  return {
    eventId: "evt-1",
    observedAt: "2026-10-07T00:00:00Z",
    kind,
    payload: {
      type: "native",
      topic: "diagnostic",
      nativeName: "resume_staging",
      status: { state: "known", value: "skipped-sidecars" },
      severity: "warning",
      relatedIds: { severity: "warning", skippedSidecars: "symlink:S/a, non-regular:S/b " },
      ...over,
    },
  };
}

describe("parseSkippedEntries", () => {
  it("splits, trims and drops empties", () => {
    expect(parseSkippedEntries("symlink:S/a, non-regular:S/b ,,")).toEqual([
      "symlink:S/a",
      "non-regular:S/b",
    ]);
  });
  it("never throws on non-string input", () => {
    expect(parseSkippedEntries(undefined)).toEqual([]);
    expect(parseSkippedEntries(42)).toEqual([]);
    expect(parseSkippedEntries(null)).toEqual([]);
  });
});

describe("skippedSidecarNotices", () => {
  it("extracts the matching diagnostic only", () => {
    const foreign = diag({ nativeName: "model_pin_mismatch" });
    const wrongStatus = diag({ status: { state: "known", value: "other" } });
    const empty = diag({ relatedIds: {} });
    const good = diag();
    const notices = skippedSidecarNotices([foreign, wrongStatus, empty, good, null, 42] as unknown[]);
    expect(notices).toHaveLength(1);
    expect(notices[0].entries).toEqual(["symlink:S/a", "non-regular:S/b"]);
    expect(notices[0].eventId).toBe("evt-1");
  });

  it("ignores non-lifecycle and malformed payloads", () => {
    expect(skippedSidecarNotices([{ kind: "model" }, {}] as unknown[])).toEqual([]);
  });
});

describe("skippedSidecarEntries", () => {
  it("deduplicates entries across notices, first-seen order kept", () => {
    const a = diag();
    const b = {
      ...(diag() as object),
      payload: {
        type: "native",
        topic: "diagnostic",
        nativeName: "resume_staging",
        status: { state: "known", value: "skipped-sidecars" },
        relatedIds: { skippedSidecars: "non-regular:S/b, symlink:S/c" },
      },
    };
    expect(skippedSidecarEntries([a, b])).toEqual([
      "symlink:S/a",
      "non-regular:S/b",
      "symlink:S/c",
    ]);
  });
});
