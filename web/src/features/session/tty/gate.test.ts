import { describe, expect, it } from "vitest";
import { instanceHasTtyAttach } from "./gate";
import { isTtyLabFixtureId, TTY_LAB_INSTANCE_ID, ttyLabInstance } from "./fixture";

describe("tty lab gate", () => {
  it("requires capabilities.tty-attach on the pty fixture and not on print", () => {
    const pty = ttyLabInstance();
    expect(instanceHasTtyAttach(pty)).toBe(true);
    expect(pty.driver).toBe("claude-pty");
    expect(isTtyLabFixtureId(TTY_LAB_INSTANCE_ID)).toBe(true);
    expect(isTtyLabFixtureId("ins_other")).toBe(false);
  });
});
