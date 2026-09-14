import { describe, expect, it } from "vitest";
import { canShowTerminal, hasStructuredSignal, instanceHasTtyAttach, isPtyBacked } from "./gate";
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
    // A native-PTY claude session keeps its terminal projection (D-028 §1.0).
    expect(canShowTerminal({ ...pty, kind: "claude", driver: "shell-pty" })).toBe(true);
  });

  it("hides the terminal on capability, never on driver name: print stays out without tty-attach", () => {
    const print = ttyLabInstance();
    print.driver = "claude-print";
    print.capabilities.capabilities["tty-attach"] = {
      state: "unsupported",
      scope: [],
      reasonCode: "print-has-no-tui",
      prerequisites: [],
      evidence: [],
    };
    expect(canShowTerminal(print)).toBe(false);
  });

  it("a driver reporting tty-attach supported gets the terminal even if it is named print", () => {
    // Capability is the decision; the string "claude-print" is not.
    const reported = ttyLabInstance();
    reported.driver = "claude-print";
    expect(canShowTerminal(reported)).toBe(true);
  });

  it("structured projection follows the signal tier", () => {
    const pty = ttyLabInstance();
    pty.kind = "claude";
    pty.driver = "shell-pty";
    // No tier yet, no structured cap: screen projection only.
    expect(hasStructuredSignal(pty)).toBe(false);
    pty.nativeRef.signalTier = "hook";
    expect(hasStructuredSignal(pty)).toBe(true);
    pty.nativeRef.signalTier = "screen";
    expect(hasStructuredSignal(pty)).toBe(false);
  });
});
