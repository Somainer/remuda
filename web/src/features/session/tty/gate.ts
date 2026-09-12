import type { Instance } from "../../../types/instance";

export function devTtyEnabled(): boolean {
  return import.meta.env.VITE_DEV_TTY === "1";
}

export function instanceHasTtyAttach(instance: Instance): boolean {
  return instance.capabilities.capabilities["tty-attach"]?.state === "supported";
}

/** Dev-only lab entry: VITE_DEV_TTY=1 and capabilities.ttyAttach (plan M0-14). */
export function canShowTtyLab(instance: Instance): boolean {
  return devTtyEnabled() && instanceHasTtyAttach(instance) && instance.driver !== "claude-print";
}
