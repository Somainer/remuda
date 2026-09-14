import { describe, expect, it } from "vitest";
import type { Observation } from "../types/generated";
import { bindingChipText, transcriptBinding } from "./transcriptBinding";

function nativeLifecycle(nativeName: string, relatedIds: Record<string, string>, nativeId?: string) {
  return {
    kind: "lifecycle",
    payload: {
      type: "native",
      nativeName,
      nativeId: nativeId ? { state: "known", value: nativeId } : { state: "not-applicable" },
      status: { state: "known", value: nativeName },
      relatedIds,
    },
  } as unknown as Observation;
}

describe("promoted transcript binding", () => {
  it("is null before any transcript lifecycle", () => {
    expect(transcriptBinding([])).toBeNull();
  });

  it("reports an unbound epoch with no channel", () => {
    const events = [nativeLifecycle("transcript_unbound", {})];
    expect(transcriptBinding(events)).toEqual({ state: "unbound" });
    expect(bindingChipText(transcriptBinding(events)!)).toBe("未绑定 transcript");
  });

  it("reports the exact bound session and which channel proved it", () => {
    const events = [
      nativeLifecycle("transcript_bound", {
        sessionId: "abc12345-0000-4000-8000-000000000000",
        transcriptPath: "/tmp/proj/abc12345.jsonl",
        source: "pid",
      }),
    ];
    const binding = transcriptBinding(events);
    expect(binding?.state).toBe("bound");
    expect(bindingChipText(binding!)).toContain("transcript abc12345");
    expect(bindingChipText(binding!)).toContain("pid 文件");
  });

  it("a later degraded announcement overrides an earlier bound one", () => {
    const events = [
      nativeLifecycle("transcript_bound", {
        sessionId: "abc12345-0000-4000-8000-000000000000",
        source: "hook",
      }),
      nativeLifecycle("transcript_degraded", {
        sessionId: "abc12345-0000-4000-8000-000000000000",
        source: "hook",
        reason: "vanished",
      }),
    ];
    const binding = transcriptBinding(events);
    expect(binding?.state).toBe("degraded");
    expect(bindingChipText(binding!)).toContain("已失效");
  });

  it("ignores unrelated lifecycle events", () => {
    const events = [
      nativeLifecycle("agent_promoted", { kind: "claude", mode: "promoted" }),
      nativeLifecycle("agent_status", {}),
    ];
    expect(transcriptBinding(events)).toBeNull();
  });
});
