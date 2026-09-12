import type { Instance } from "../../../types/instance";
import { uiMode } from "../../../lib/status";

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

/** Terminal tab: pty-backed kinds, or the VITE_DEV_TTY lab fixture. */
export function canShowTerminal(instance: Instance): boolean {
  if (instance.driver === "claude-print" && instance.kind !== "terminal") return false;
  if (isPtyBacked(instance)) return true;
  return instanceHasTtyAttach(instance) && uiMode(instance) === "tty-attachable";
}

/** @deprecated lab name; same gate as {@link canShowTerminal}. */
export function canShowTtyLab(instance: Instance): boolean {
  return canShowTerminal(instance);
}
