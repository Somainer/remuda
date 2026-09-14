export type PermissionDefault = "manual" | "acceptEdits" | "dontAsk" | "bypassPermissions";

export type DeviceSettings = {
  deviceName: string;
  autoRevealTty: boolean;
  permissionDefault: PermissionDefault;
  theme: "night-corral";
  /** Index into the claude native table; remapped by nearest index when the harness changes. */
  defaultEffortIndex: number;
};

const KEY = "runtime.device-settings.v1";

export const DEFAULT_SETTINGS: DeviceSettings = {
  deviceName: "this-device",
  autoRevealTty: false,
  permissionDefault: "manual",
  theme: "night-corral",
  defaultEffortIndex: 2,
};

export function readDeviceSettings(): DeviceSettings {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    const parsed = JSON.parse(raw) as Partial<DeviceSettings>;
    const permissionDefault: PermissionDefault =
      parsed.permissionDefault === "acceptEdits" ||
      parsed.permissionDefault === "dontAsk" ||
      parsed.permissionDefault === "bypassPermissions"
        ? parsed.permissionDefault
        : "manual";
    const effortRaw = Number(parsed.defaultEffortIndex);
    return {
      deviceName: parsed.deviceName?.trim() || DEFAULT_SETTINGS.deviceName,
      autoRevealTty: parsed.autoRevealTty === true,
      permissionDefault,
      theme: "night-corral",
      defaultEffortIndex: Number.isFinite(effortRaw) ? Math.max(0, Math.min(4, Math.round(effortRaw))) : 2,
    };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

export function writeDeviceSettings(patch: Partial<DeviceSettings>): DeviceSettings {
  const next: DeviceSettings = { ...readDeviceSettings(), ...patch, theme: "night-corral" };
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    /* ignore quota */
  }
  return next;
}

export function iosStandaloneHint(): string {
  return "iOS 需把 runtime 加到主屏幕（独立 PWA）后，系统才允许 Notification。";
}
