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
  it("idle: primary is plain send, queue/interrupt/steer hidden", () => {
    const s = composerState("claude", "idle", caps({ steer: cap("supported", "native") }));
    expect(s.primary).toEqual({ kind: "send", label: "发送", mode: "new-turn" });
    expect(s.queue.available).toBe(false);
    expect(s.interrupt.available).toBe(false);
    expect(s.steer.available).toBe(false);
  });

  it("exited: same single send (the dock is disabled by the caller)", () => {
    const s = composerState("claude", "exited", caps());
    expect(s.primary.kind).toBe("send");
    expect(s.queue.available).toBe(false);
    expect(s.steer.available).toBe(false);
  });

  it("working: Enter queues (Remuda-held), 插队 is the second action, 打断 present", () => {
    const s = composerState(
      "claude",
      "working",
      caps({ steer: cap(), queue: cap("supported", "emulated"), interrupt: cap() }),
    );
    expect(s.primary).toMatchObject({ kind: "queue", mode: "queue", holder: "remuda" });
    expect(s.queue).toMatchObject({ available: true, holder: "remuda" });
    expect(s.interrupt).toMatchObject({ available: true, provision: "native", label: "打断" });
    expect(s.steer).toMatchObject({ available: true, provision: "native", label: "插队" });
  });

  it("working + native queue (codex Tab): the primary hold is posted natively", () => {
    const s = composerState(
      "codex",
      "working",
      caps({ steer: cap(), queue: cap("supported", "native"), interrupt: cap() }),
    );
    expect(s.primary).toMatchObject({ kind: "queue", holder: "native" });
    expect(s.queue).toMatchObject({ available: true, holder: "native" });
    expect(s.steer.available).toBe(true);
  });

  it("working + emulated interrupt: 插队 honestly says Remuda sends the cancel sequence", () => {
    const s = composerState(
      "grok",
      "working",
      caps({ steer: cap("supported", "emulated"), queue: cap("supported", "emulated"), interrupt: cap("supported", "emulated") }),
    );
    expect(s.primary).toMatchObject({ kind: "queue", holder: "remuda" });
    expect(s.steer).toMatchObject({ available: true, provision: "emulated" });
    expect(s.interrupt).toMatchObject({ provision: "emulated", note: "Remuda 代发取消序列" });
  });

  it("working + unknown interrupt: 插队 still offered, honestly labelled 尚未验证", () => {
    const empty = caps({});
    const s = composerState("agy", "working", empty);
    const t = triple(empty);
    expect(provision(t.steer)).toBe("unknown");
    expect(s.primary.kind).toBe("queue");
    expect(s.steer).toMatchObject({ available: true, provision: "unknown", note: "尚未验证" });
    expect(s.interrupt).toMatchObject({ available: true, provision: "unknown" });
  });

  it("an explicitly unsupported interrupt removes both 打断 and 插队 but keeps the queue", () => {
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
    expect(s.steer.available).toBe(false);
    expect(s.interrupt.available).toBe(false);
  });

  it("the queue is available even when its capability is unknown (Remuda holds it)", () => {
    const s = composerState("grok", "working", caps({ steer: cap("supported", "emulated") }));
    expect(s.primary).toMatchObject({ kind: "queue", holder: "remuda" });
    expect(s.queue).toMatchObject({ available: true, holder: "remuda" });
  });

  it("blocked (pending question/approval): held until answered, NO 插队 and no Esc-to-dialog", () => {
    const s = composerState(
      "claude",
      "blocked",
      caps({ steer: cap(), queue: cap("supported", "emulated"), interrupt: cap() }),
    );
    expect(s.primary).toMatchObject({ kind: "queue", holder: "remuda", waitNote: "待回答后送出" });
    expect(s.steer.available).toBe(false);
    // Plain interrupt stays possible (the user may still abandon the turn).
    expect(s.interrupt.available).toBe(true);
    expect(s.note).toContain("回答后送出");
  });
});

describe("measured per-kind static matrix", () => {
  it("claude shell-pty: Remuda-held Enter queue, native-Esc 插队", () => {
    const s = composerState("claude", "working", agentPtyCapabilities("claude", "shell-pty"));
    expect(s.primary).toMatchObject({ kind: "queue", holder: "remuda" });
    expect(s.steer).toMatchObject({ provision: "native" });
    expect(s.interrupt).toMatchObject({ provision: "native" });
  });

  it("codex shell-pty: native Enter queue", () => {
    const s = composerState("codex", "working", agentPtyCapabilities("codex", "shell-pty"));
    expect(s.primary).toMatchObject({ kind: "queue", holder: "native" });
    expect(s.steer).toMatchObject({ provision: "native" });
  });

  it("grok shell-pty: emulated 插队/interrupt, Remuda-held queue", () => {
    const s = composerState("grok", "working", agentPtyCapabilities("grok", "shell-pty"));
    expect(s.primary).toMatchObject({ holder: "remuda" });
    expect(s.steer).toMatchObject({ provision: "emulated" });
    expect(s.interrupt).toMatchObject({ provision: "emulated" });
  });

  it("agy shell-pty: everything unknown and honestly labelled", () => {
    const s = composerState("agy", "working", agentPtyCapabilities("agy", "shell-pty"));
    expect(s.steer).toMatchObject({ provision: "unknown", note: "尚未验证" });
    expect(s.interrupt).toMatchObject({ provision: "unknown" });
  });

  it("idle stays a plain send for every measured harness", () => {
    for (const kind of ["claude", "codex", "grok", "agy"] as const) {
      const s = composerState(kind, "idle", agentPtyCapabilities(kind, "shell-pty"));
      expect(s.primary).toEqual({ kind: "send", label: "发送", mode: "new-turn" });
    }
  });
});
