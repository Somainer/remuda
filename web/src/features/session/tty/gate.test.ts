import { describe, expect, it } from "vitest";
import { canShowTerminal, instanceHasTtyAttach, isPtyBacked } from "./gate";
import { isTtyLabFixtureId, TTY_LAB_INSTANCE_ID, ttyLabInstance } from "./fixture";

describe("tty lab gate", () => {
  it("requires capabilities.tty-attach on the pty fixture and not on print", () => {
    const pty = ttyLabInstance();
    expect(instanceHasTtyAttach(pty)).toBe(true);
    expect(pty.driver).toBe("claude-pty");
    expect(isTtyLabFixtureId(TTY_LAB_INSTANCE_ID)).toBe(true);
    expect(isTtyLabFixtureId("ins_other")).toBe(false);
  });

  it("treats terminal / generic-pty / agent pty kinds as tty-attachable", () => {
    const pty = ttyLabInstance();
    expect(isPtyBacked(pty)).toBe(true);
    expect(canShowTerminal(pty)).toBe(true);
    expect(canShowTerminal({ ...pty, kind: "grok", driver: "generic-pty" })).toBe(true);
    expect(canShowTerminal({ ...pty, kind: "terminal", driver: "shell-pty" })).toBe(true);
    expect(canShowTerminal({ ...pty, kind: "claude", driver: "claude-print" })).toBe(false);
  });
});
