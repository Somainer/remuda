import type { Instance } from "../../../types/instance";
import type { SignalTier } from "../../../types/nativeRef";

export function devTtyEnabled(): boolean {
  return import.meta.env.VITE_DEV_TTY === "1";
}

export function instanceHasTtyAttach(instance: Instance): boolean {
  return instance.capabilities.capabilities["tty-attach"]?.state === "supported";
}

export function isPtyBacked(instance: Instance): boolean {
  if (instance.kind === "terminal") return true;
  if (
    instance.driver === "shell-pty" ||
    instance.driver === "generic-pty" ||
    instance.driver === "claude-pty"
  ) {
    return true;
  }
  if (instance.kind === "codex" || instance.kind === "grok" || instance.kind === "agy") {
    return instance.driver !== "claude-print";
  }
  return false;
}

/**
 * Terminal projection (D-016 raw bytes): any PTY-backed session, or a driver
 * that explicitly supports tty-attach. The decision is about the carrier and
 * the reported capability — never about the driver *name*: a `claude-print`
 * row hides the tab because `tty-attach` is unsupported, not because of a
 * string compare (D-028 §1.0).
 */
export function canShowTerminal(instance: Instance): boolean {
  if (isPtyBacked(instance)) return true;
  return instanceHasTtyAttach(instance);
}

/** Tiers that carry a structured (non-screen) signal; D-028 §4.3. */
const STRUCTURED_TIERS: ReadonlySet<SignalTier> = new Set(["hook", "file", "osc"]);

/**
 * Structured projection: the session has (or is statically known to provide)
 * a structured signal tier. `screen`-only sessions render the screen
 * projection instead; `claude-print` is structured by construction even with
 * no runtime `signalTier` yet.
 */
export function hasStructuredSignal(instance: Instance): boolean {
  const tier = instance.nativeRef.signalTier;
  if (tier) return STRUCTURED_TIERS.has(tier);
  if (instance.driver === "claude-print") return true;
  if (isPromotedKind(instance)) return true;
  return instance.capabilities.capabilities["structured-workflow"]?.state === "supported";
}

/** A promoted terminal (D-025) hydrates a transcript even on shell-pty. */
function isPromotedKind(instance: Instance): boolean {
  return instance.mode === "promoted" && instance.kind !== "terminal";
}

/** @deprecated lab name; same gate as {@link canShowTerminal}. */
export function canShowTtyLab(instance: Instance): boolean {
  return canShowTerminal(instance);
}
