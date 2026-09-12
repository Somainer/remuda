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
const LEGACY_ACCESS_KEY = "runtime.access-code";
let memorySession: DeviceSession | null = null;

export const MOCK_BOOTSTRAP_TOKEN = "dev-bootstrap";

export function readSession(): DeviceSession | null {
  try {
    // Previous builds persisted the same bearer secret under two keys.
    localStorage.removeItem(LEGACY_ACCESS_KEY);
    const raw = localStorage.getItem(SESSION_KEY);
    if (!raw) return memorySession;
    const parsed = JSON.parse(raw) as Partial<DeviceSession>;
    if (typeof parsed.deviceId !== "string" || !parsed.deviceId || typeof parsed.name !== "string" || !parsed.name) return null;
    const metadata = { deviceId: parsed.deviceId, name: parsed.name };
    if ("token" in parsed) localStorage.setItem(SESSION_KEY, JSON.stringify(metadata));
    return { ...metadata, token: memorySession?.deviceId === parsed.deviceId ? memorySession.token : "" };
  } catch {
    return memorySession;
  }
}

export function writeSession(session: DeviceSession): void {
  memorySession = { ...session };
  try {
    localStorage.removeItem(LEGACY_ACCESS_KEY);
    localStorage.setItem(SESSION_KEY, JSON.stringify({ deviceId: session.deviceId, name: session.name }));
    localStorage.removeItem(LOGGED_OUT_KEY);
    localStorage.setItem(DEVICE_KEY, session.deviceId);
  } catch {
    /* ignore quota */
  }
}

export function clearSession(): void {
  memorySession = null;
  try {
    localStorage.removeItem(LEGACY_ACCESS_KEY);
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
  // The Hub clears the HttpOnly cookie on device revocation. JavaScript cannot
  // read or delete it; clearSession separately drops the in-memory credential.
}
