import { describe, expect, it } from "vitest";
import {
  detectPlatform,
  modifierAriaShortcut,
  modifierBadgeText,
  primaryModifierHeld,
  setPlatformForTest,
  platformInfo,
} from "./platform";

describe("detectPlatform", () => {
  it("treats userAgentData.platform as the primary signal", () => {
    expect(detectPlatform({ userAgentData: { platform: "macOS" }, platform: "Win32" }).kind).toBe("mac");
    expect(detectPlatform({ userAgentData: { platform: "Windows" }, platform: "MacIntel" }).kind).toBe("desktop");
    expect(detectPlatform({ userAgentData: { platform: "Linux" } }).kind).toBe("desktop");
  });

  it("falls back to navigator.platform strings", () => {
    expect(detectPlatform({ platform: "MacIntel" }).kind).toBe("mac");
    expect(detectPlatform({ platform: "Win32" }).kind).toBe("desktop");
    expect(detectPlatform({ platform: "Linux x86_64" }).kind).toBe("desktop");
    expect(detectPlatform({ platform: "" }).kind).toBe("desktop");
    expect(detectPlatform(undefined).kind).toBe("desktop");
  });

  it("exposes Meta on macOS and Control on other desktops", () => {
    const mac = detectPlatform({ platform: "MacIntel" });
    expect(mac.modifierKey).toBe("Meta");
    expect(mac.glyph).toBe("⌘");
    expect(mac.ariaModifier).toBe("Meta");
    expect(mac.heldModifiersSupported).toBe(true);

    const win = detectPlatform({ platform: "Win32" });
    expect(win.modifierKey).toBe("Control");
    expect(win.glyph).toBe("Ctrl");
    expect(win.ariaModifier).toBe("Control");
    expect(win.heldModifiersSupported).toBe(true);
  });

  it("detects iPhone / iPad / Android as touch with no held-modifier flow", () => {
    // UA-CH platforms first...
    expect(detectPlatform({ userAgentData: { platform: "Android" }, platform: "Linux armv8l" }).kind).toBe("touch");
    expect(detectPlatform({ userAgentData: { platform: "iPhone" } }).kind).toBe("touch");
    // ...then the legacy navigator.platform fallbacks.
    for (const platform of ["iPhone", "iPad", "iPod", "Linux armv7l", "Linux armv8l", "Linux aarch64"]) {
      const info = detectPlatform({ platform });
      expect(info.kind).toBe("touch");
      expect(info.heldModifiersSupported).toBe(false);
    }
    // Desktop Linux stays a desktop despite the "Linux" prefix.
    expect(detectPlatform({ platform: "Linux x86_64" }).kind).toBe("desktop");
  });

  it("sees through the iPadOS desktop-Mac masquerade via touch points", () => {
    expect(detectPlatform({ platform: "MacIntel", maxTouchPoints: 5 }).kind).toBe("touch");
    expect(detectPlatform({ userAgentData: { platform: "macOS" }, platform: "MacIntel", maxTouchPoints: 0 }).kind).toBe("mac");
  });
});

describe("platform formatting helpers", () => {
  it("renders badge text with the platform glyph", () => {
    setPlatformForTest(detectPlatform({ platform: "MacIntel" }));
    expect(modifierBadgeText(1)).toBe("⌘ 1");
    expect(modifierAriaShortcut(9)).toBe("Meta+9");

    setPlatformForTest(detectPlatform({ platform: "Win32" }));
    expect(modifierBadgeText(3)).toBe("Ctrl 3");
    expect(modifierAriaShortcut(3)).toBe("Control+3");

    setPlatformForTest(null);
  });

  it("checks the modifier matching the platform", () => {
    const mac = detectPlatform({ platform: "MacIntel" });
    const win = detectPlatform({ platform: "Win32" });
    expect(primaryModifierHeld({ metaKey: true }, mac)).toBe(true);
    expect(primaryModifierHeld({ ctrlKey: true }, mac)).toBe(false);
    expect(primaryModifierHeld({ ctrlKey: true }, win)).toBe(true);
    expect(primaryModifierHeld({ metaKey: true }, win)).toBe(false);
  });

  it("caches one detection per process and lets tests reset it", () => {
    setPlatformForTest(detectPlatform({ platform: "MacIntel" }));
    expect(platformInfo().glyph).toBe("⌘");
    expect(platformInfo().glyph).toBe("⌘");
    setPlatformForTest(null);
  });
});
