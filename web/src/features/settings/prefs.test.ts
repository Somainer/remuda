import { afterEach, describe, expect, it } from "vitest";
import { DEFAULT_SETTINGS, iosStandaloneHint, readDeviceSettings, writeDeviceSettings } from "./prefs";

afterEach(() => {
  localStorage.removeItem("runtime.device-settings.v1");
});

describe("device settings", () => {
  it("defaults autoRevealTty off and Night Corral only", () => {
    expect(DEFAULT_SETTINGS.autoRevealTty).toBe(false);
    expect(DEFAULT_SETTINGS.theme).toBe("night-corral");
    expect(DEFAULT_SETTINGS.permissionDefault).toBe("manual");
    // The five real Claude levels default to `high`, index 2.
    expect(DEFAULT_SETTINGS.defaultEffortIndex).toBe(2);
    expect(readDeviceSettings().autoRevealTty).toBe(false);
  });

  it("clamps a stored effort index onto the five-level table", () => {
    localStorage.setItem("runtime.device-settings.v1", JSON.stringify({ defaultEffortIndex: 9 }));
    expect(readDeviceSettings().defaultEffortIndex).toBe(4);
    localStorage.setItem("runtime.device-settings.v1", JSON.stringify({ defaultEffortIndex: -3 }));
    expect(readDeviceSettings().defaultEffortIndex).toBe(0);
    // A non-finite stored value falls back to the high default.
    localStorage.setItem("runtime.device-settings.v1", JSON.stringify({ defaultEffortIndex: "x" }));
    expect(readDeviceSettings().defaultEffortIndex).toBe(2);
  });

  it("persists device name and permission default without enabling tty auto-reveal", () => {
    writeDeviceSettings({ deviceName: "phone", permissionDefault: "acceptEdits" });
    const next = readDeviceSettings();
    expect(next.deviceName).toBe("phone");
    expect(next.permissionDefault).toBe("acceptEdits");
    expect(next.autoRevealTty).toBe(false);
    expect(next.theme).toBe("night-corral");
  });

  it("mentions iOS home screen for push", () => {
    expect(iosStandaloneHint()).toMatch(/主屏幕/);
  });
});
