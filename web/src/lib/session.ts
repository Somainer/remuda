import { DEVICE_KEY } from "./interactionStatus";

export type DeviceSession = {
  deviceId: string;
  token: string;
  name: string;
};

export type PairedDevice = {
  id: string;
  name: string;
};

export type PairCode = {
  code: string;
  expiresAt: string;
};

const SESSION_KEY = "runtime.device-session";
const LOGGED_OUT_KEY = "runtime.logged-out";

export const MOCK_BOOTSTRAP_TOKEN = "dev-bootstrap";

export function readSession(): DeviceSession | null {
  try {
    const raw = localStorage.getItem(SESSION_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<DeviceSession>;
    if (!parsed.deviceId || !parsed.token || !parsed.name) return null;
    return { deviceId: parsed.deviceId, token: parsed.token, name: parsed.name };
  } catch {
    return null;
  }
}

export function writeSession(session: DeviceSession): void {
  try {
    localStorage.setItem(SESSION_KEY, JSON.stringify(session));
    localStorage.removeItem(LOGGED_OUT_KEY);
    localStorage.setItem(DEVICE_KEY, session.deviceId);
  } catch {
    /* ignore quota */
  }
}

export function clearSession(): void {
  try {
    localStorage.removeItem(SESSION_KEY);
    localStorage.setItem(LOGGED_OUT_KEY, "1");
  } catch {
    /* ignore */
  }
}

export function readLoggedOut(): boolean {
  try {
    return localStorage.getItem(LOGGED_OUT_KEY) === "1";
  } catch {
    return false;
  }
}

export function rememberDeviceId(deviceId: string): void {
  try {
    localStorage.setItem(DEVICE_KEY, deviceId);
  } catch {
    /* ignore */
  }
}

export function dropDeviceCookie(): void {
  try {
    document.cookie = "remuda_device=; Path=/; Max-Age=0; SameSite=Strict";
  } catch {
    /* ignore */
  }
}
