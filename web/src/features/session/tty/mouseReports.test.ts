import { describe, expect, it } from "vitest";
import {
  allowInput,
  inputGate,
  isMouseReport,
  localWheelWanted,
  MOUSE_TRACKING_RESET,
  trackingActive,
} from "./mouseReports";

const SGR_PRESS = "[<0;22;8M";
const SGR_RELEASE = "[<0;22;8m";
const SGR_WHEEL = "[<64;77;22M";
const X10 = "[M 0(";

describe("isMouseReport", () => {
  it("recognises SGR press, release and wheel reports", () => {
    expect(isMouseReport(SGR_PRESS)).toBe(true);
    expect(isMouseReport(SGR_RELEASE)).toBe(true);
    expect(isMouseReport(SGR_WHEEL)).toBe(true);
  });

  it("recognises X10 and urxvt encodings", () => {
    expect(isMouseReport(X10)).toBe(true);
    expect(isMouseReport("[32;22;8M")).toBe(true);
  });

  it("does not mistake keystrokes or cursor keys for mouse reports", () => {
    for (const data of ["a", "echo ok", "\r", "", "", "[A", "[5~", "OA"]) {
      expect(isMouseReport(data)).toBe(false);
    }
  });
});

describe("inputGate", () => {
  it("passes both keyboard and mouse in direct mode with reports on", () => {
    expect(inputGate({ directInput: true, frozen: false, mouseReports: true })).toEqual({
      keyboard: true,
      mouse: true,
    });
  });

  it("keeps the mouse alive in keys mode (A3: narrow viewport)", () => {
    expect(inputGate({ directInput: false, frozen: false, mouseReports: true })).toEqual({
      keyboard: false,
      mouse: true,
    });
  });

  it("drops pointer reports when the user turns 鼠标上报 off (A2)", () => {
    expect(inputGate({ directInput: true, frozen: false, mouseReports: false })).toEqual({
      keyboard: true,
      mouse: false,
    });
  });

  it("blocks everything while frozen", () => {
    expect(inputGate({ directInput: true, frozen: true, mouseReports: true })).toEqual({
      keyboard: false,
      mouse: false,
    });
  });
});

describe("allowInput", () => {
  it("lets a click through in keys mode but swallows the keystroke", () => {
    const gate = inputGate({ directInput: false, frozen: false, mouseReports: true });
    expect(allowInput(SGR_PRESS, gate)).toBe(true);
    expect(allowInput("a", gate)).toBe(false);
  });

  it("swallows the wheel report but keeps typing when reports are off", () => {
    const gate = inputGate({ directInput: true, frozen: false, mouseReports: false });
    expect(allowInput(SGR_WHEEL, gate)).toBe(false);
    expect(allowInput("echo ok\r", gate)).toBe(true);
  });
});

describe("trackingActive / localWheelWanted", () => {
  it("treats any non-none tracking mode as active", () => {
    expect(trackingActive("none")).toBe(false);
    for (const mode of ["x10", "vt200", "drag", "any"]) expect(trackingActive(mode)).toBe(true);
  });

  it("takes the wheel back only when the app tracks and reports are off", () => {
    expect(localWheelWanted({ mouseMode: "vt200", mouseReports: false })).toBe(true);
    // Reports on: xterm's own report path should win.
    expect(localWheelWanted({ mouseMode: "vt200", mouseReports: true })).toBe(false);
    // No tracking: xterm already scrolls the scrollback itself.
    expect(localWheelWanted({ mouseMode: "none", mouseReports: false })).toBe(false);
  });
});

describe("MOUSE_TRACKING_RESET", () => {
  it("sends DECRST for every tracking mode plus SGR encoding", () => {
    expect(MOUSE_TRACKING_RESET).toBe("[?1000l[?1002l[?1003l[?1006l");
  });
});
