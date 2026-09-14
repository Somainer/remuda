import { describe, expect, it } from "vitest";
import { composerState, provision, triple } from "./state";
import { printCapabilities, agentPtyCapabilities } from "../../lib/capabilities";
import type { Capability, CapabilityProvision, CapabilitySnapshot } from "../../types/nativeRef";

function caps(
  over: Partial<Record<"steer" | "queue" | "interrupt", Capability>> = {},
): CapabilitySnapshot {
  const base = printCapabilities();
  return {
    ...base,
    capabilities: { ...base.capabilities, ...over } as CapabilitySnapshot["capabilities"],
  };
}

function cap(state: Capability["state"] = "supported", p: CapabilityProvision = "native"): Capability {
  return { state, provision: p, scope: [], reasonCode: "test", prerequisites: [], evidence: [] };
}

describe("composer state machine", () => {
  it("idle: primary is plain send, queue/interrupt hidden", () => {
    const s = composerState("claude", "idle", caps({ steer: cap("supported", "native") }));
    expect(s.primary).toEqual({ kind: "send", label: "发送", mode: "new-turn" });
    expect(s.queue.available).toBe(false);
    expect(s.interrupt.available).toBe(false);
    expect(s.interruptAndSend).toBe(false);
  });

  it("exited: same single send (the dock is disabled by the caller)", () => {
    const s = composerState("claude", "exited", caps());
    expect(s.primary.kind).toBe("send");
    expect(s.queue.available).toBe(false);
  });

  it("working + native steer: steer primary, queue always available, interrupt present", () => {
    const s = composerState(
      "claude",
      "working",
      caps({ steer: cap(), queue: cap("supported", "emulated"), interrupt: cap() }),
    );
    expect(s.primary).toMatchObject({ kind: "steer", mode: "steer" });
    expect(s.queue).toMatchObject({ available: true, holder: "remuda" });
    expect(s.interrupt).toMatchObject({ available: true, provision: "native", label: "打断" });
    expect(s.interruptAndSend).toBe(false);
    expect(s.note).toContain("工具边界");
  });

  it("working + native queue (codex Tab): labelled as a native queue", () => {
    const s = composerState(
      "codex",
      "working",
      caps({ steer: cap(), queue: cap("supported", "native"), interrupt: cap() }),
    );
    expect(s.primary.kind).toBe("steer");
    expect(s.queue).toMatchObject({ available: true, holder: "native" });
  });

  it("working + emulated steer: queue primary, send becomes interrupt-and-send", () => {
    const s = composerState(
      "grok",
      "working",
      caps({ steer: cap("supported", "emulated"), queue: cap("supported", "emulated"), interrupt: cap("supported", "emulated") }),
    );
    expect(s.primary).toMatchObject({ kind: "queue", mode: "queue" });
    expect(s.interruptAndSend).toBe(true);
    expect(s.note).toContain("取消当前 turn");
    expect(s.interrupt).toMatchObject({ provision: "emulated", note: "Remuda 代发取消序列" });
  });

  it("working + unknown steer: queue primary, interrupt-and-send honest 尚未验证", () => {
    const empty = caps({});
    const s = composerState("agy", "working", empty);
    const t = triple(empty);
    expect(provision(t.steer)).toBe("unknown");
    expect(s.primary.kind).toBe("queue");
    expect(s.interruptAndSend).toBe(true);
    expect(s.note).toContain("尚未验证");
    // Unknown interrupt is still offered, labelled unverified — not fake-grey.
    expect(s.interrupt).toMatchObject({ available: true, provision: "unknown" });
  });

  it("explicit unsupported steer removes interrupt-and-send but keeps the queue", () => {
    const s = composerState(
      "agy",
      "working",
      caps({
        steer: cap("unsupported", "unknown"),
        queue: cap("supported", "emulated"),
        interrupt: cap("unsupported", "unknown"),
      }),
    );
    expect(s.primary.kind).toBe("queue");
    expect(s.interruptAndSend).toBe(false);
    expect(s.interrupt.available).toBe(false);
    expect(s.queue.available).toBe(true);
  });

  it("the queue is available even when its capability is unknown (Remuda holds it)", () => {
    const s = composerState("grok", "working", caps({ steer: cap("supported", "emulated") }));
    expect(s.queue).toMatchObject({ available: true, holder: "remuda" });
  });

  it("blocked: plain send into the D-022 queue, interrupt available", () => {
    const s = composerState(
      "claude",
      "blocked",
      caps({ steer: cap(), queue: cap("supported", "emulated"), interrupt: cap() }),
    );
    expect(s.primary).toEqual({ kind: "send", label: "发送", mode: "new-turn" });
    expect(s.queue.available).toBe(false);
    expect(s.interrupt.available).toBe(true);
  });
});

describe("measured per-kind static matrix", () => {
  it("claude shell-pty: native steer/interrupt, Remuda-held queue", () => {
    const s = composerState("claude", "working", agentPtyCapabilities("claude", "shell-pty"));
    expect(s.primary.kind).toBe("steer");
    expect(s.queue).toMatchObject({ holder: "remuda" });
    expect(s.interrupt).toMatchObject({ provision: "native" });
  });

  it("codex shell-pty: native steer/queue/interrupt", () => {
    const s = composerState("codex", "working", agentPtyCapabilities("codex", "shell-pty"));
    expect(s.primary.kind).toBe("steer");
    expect(s.queue).toMatchObject({ holder: "native" });
    expect(s.interrupt).toMatchObject({ provision: "native" });
  });

  it("grok shell-pty: emulated steer/interrupt, Remuda-held queue", () => {
    const s = composerState("grok", "working", agentPtyCapabilities("grok", "shell-pty"));
    expect(s.interruptAndSend).toBe(true);
    expect(s.queue).toMatchObject({ holder: "remuda" });
    expect(s.interrupt).toMatchObject({ provision: "emulated" });
  });

  it("agy shell-pty: everything unknown and honestly labelled", () => {
    const s = composerState("agy", "working", agentPtyCapabilities("agy", "shell-pty"));
    expect(s.note).toContain("尚未验证");
    expect(s.interrupt).toMatchObject({ provision: "unknown" });
  });

  it("idle stays a plain send for every measured harness", () => {
    for (const kind of ["claude", "codex", "grok", "agy"] as const) {
      const s = composerState(kind, "idle", agentPtyCapabilities(kind, "shell-pty"));
      expect(s.primary).toEqual({ kind: "send", label: "发送", mode: "new-turn" });
    }
  });
});
