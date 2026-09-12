export type PermissionDefault = "manual" | "acceptEdits" | "bypassPermissions";

export type DeviceSettings = {
  deviceName: string;
  autoRevealTty: boolean;
  permissionDefault: PermissionDefault;
  theme: "night-corral";
};

const KEY = "runtime.device-settings.v1";

export const DEFAULT_SETTINGS: DeviceSettings = {
  deviceName: "this-device",
  autoRevealTty: false,
  permissionDefault: "manual",
  theme: "night-corral",
};

export function readDeviceSettings(): DeviceSettings {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    const parsed = JSON.parse(raw) as Partial<DeviceSettings>;
    return {
      deviceName: parsed.deviceName?.trim() || DEFAULT_SETTINGS.deviceName,
      autoRevealTty: parsed.autoRevealTty === true,
      permissionDefault:
        parsed.permissionDefault === "acceptEdits" || parsed.permissionDefault === "bypassPermissions"
          ? parsed.permissionDefault
          : "manual",
      theme: "night-corral",
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
