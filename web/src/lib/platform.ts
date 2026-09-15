/**
 * Platform detection, done once, in one place.
 *
 * The held-modifier session switcher has to say "⌘" on macOS and "Ctrl"
 * everywhere else, listen for the matching modifier, and stay silent on
 * touch platforms where there is no held-modifier flow at all. Components
 * must not sniff the user agent themselves — every caller reads
 * {@link platformInfo} (or an injected {@link PlatformInfo} in tests).
 *
 * Detection order follows the owner's brief: `navigator.userAgentData.platform`
 * first, `navigator.platform` as the fallback.
 */

export type PlatformKind = "mac" | "desktop" | "touch";

export type PlatformInfo = {
  kind: PlatformKind;
  /**
   * The `event.key` value of the platform-primary modifier ("Meta" on macOS,
   * "Control" on Windows/Linux). The hook reveals badges only for this one.
   */
  modifierKey: "Meta" | "Control";
  /** Glyph shown in badges and hints: ⌘ on macOS, Ctrl elsewhere. */
  glyph: string;
  /** The `aria-keyshortcuts` token for the modifier. */
  ariaModifier: "Meta" | "Control";
  /**
   * False on iPadOS/iOS/Android: no hardware modifier is held there, so the
   * hint must not render and no listener should attach.
   */
  heldModifiersSupported: boolean;
};

const mac: PlatformInfo = {
  kind: "mac",
  modifierKey: "Meta",
  glyph: "⌘",
  ariaModifier: "Meta",
  heldModifiersSupported: true,
};

const desktop: PlatformInfo = {
  kind: "desktop",
  modifierKey: "Control",
  glyph: "Ctrl",
  ariaModifier: "Control",
  heldModifiersSupported: true,
};

/**
 * Touch keeps the Ctrl-shaped fields as harmless fallbacks, but
 * {@link PlatformInfo.heldModifiersSupported} switches every consumer off.
 */
const touch: PlatformInfo = {
  kind: "touch",
  modifierKey: "Control",
  glyph: "",
  ariaModifier: "Control",
  heldModifiersSupported: false,
};

/** The smallest navigator surface detection reads; easy to forge in tests. */
export type NavigatorLike = {
  platform?: string;
  maxTouchPoints?: number;
  userAgentData?: { platform?: string } | null;
};

/**
 * Classify the platform from an injectable navigator.
 *
 * iPadOS pretends to be a desktop Mac ("Macintosh" via UA-CH, "MacIntel" via
 * `navigator.platform`); the only tell is a touch-capable display, the same
 * heuristic `lib/pwa.ts` already uses. Everything positively identified as
 * iPhone/iPad/iPod/Android is touch; an unknown string defaults to desktop,
 * because that is the only keyboard-driven choice among the fallbacks.
 */
export function detectPlatform(nav: NavigatorLike | undefined | null = typeof navigator === "undefined" ? null : navigator): PlatformInfo {
  const raw = String(nav?.userAgentData?.platform ?? nav?.platform ?? "");
  const p = raw.toLowerCase();
  // Legacy Android WebViews predate UA-CH and report "Linux armv7l"/"Linux
  // armv8l"; desktop Linux is "Linux i686"/"Linux x86_64".
  if (/iphone|ipad|ipod|ios|android|linux (arm|aarch)/.test(p)) return touch;
  if (p.includes("mac")) {
    // iPadOS desktop-UA masquerade.
    if ((nav?.maxTouchPoints ?? 0) > 1) return touch;
    return mac;
  }
  return desktop;
}

let cached: PlatformInfo | null = null;

/** The process-wide platform, detected lazily on first use. */
export function platformInfo(): PlatformInfo {
  cached ??= detectPlatform();
  return cached;
}

/**
 * Test seam: pin the platform a component under test sees, or pass `null` to
 * go back to real detection.
 */
export function setPlatformForTest(info: PlatformInfo | null): void {
  cached = info;
}

/** True when a keyboard event carries the platform-primary modifier. */
export function primaryModifierHeld(event: { metaKey?: boolean; ctrlKey?: boolean }, platform: PlatformInfo = platformInfo()): boolean {
  return platform.modifierKey === "Meta" ? Boolean(event.metaKey) : Boolean(event.ctrlKey);
}

/** Badge text for a 1–9 slot, e.g. "⌘ 3" or "Ctrl 3". */
export function modifierBadgeText(slot: number, platform: PlatformInfo = platformInfo()): string {
  return `${platform.glyph} ${slot}`;
}

/** The permanent `aria-keyshortcuts` value for a 1–9 slot, e.g. "Control+3". */
export function modifierAriaShortcut(slot: number, platform: PlatformInfo = platformInfo()): string {
  return `${platform.ariaModifier}+${slot}`;
}
