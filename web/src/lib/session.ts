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

export function writeSession(session: DeviceSession, { mock = false } = {}): void {
  // Live browser requests authenticate with the HttpOnly cookie. Only the
  // in-process mock API needs a bearer; do not expose live tokens to WS helpers.
  memorySession = { ...session, token: mock ? session.token : "" };
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
    localStorage.removeItem(ANON_EPOCH_KEY);
    localStorage.setItem(LOGGED_OUT_KEY, "1");
  } catch {
    /* ignore */
  }
}

const ANON_EPOCH_KEY = "runtime.outbox-auth-epoch";
/**
 * Per-PAGE-LIFETIME anon epoch used only when neither localStorage persists
 * one nor it can be written (c-reconnfu gate 29 item 5): minted at most once
 * per loaded module, so repeated readAuthEpoch() calls under a failing
 * storage keep returning the same id instead of starting a new "epoch" each
 * time. A real reload intentionally gets a fresh document/module.
 */
let memoryAnonEpoch: string | null = null;

/**
 * The current AUTH EPOCH that owns outbox deliveries (c-reconnfu gate 28):
 * normally the logged-in device session id (stable across reloads of the same
 * login, changed by logout/reinit). When no device session exists (mock/test
 * harness without a login), fall back to a stable id persisted in
 * localStorage so a reload keeps the same epoch while a logout (which clears
 * it via {@link clearSession}) starts a new one.
 */
export function readAuthEpoch(): string {
  const deviceId = readSession()?.deviceId;
  if (deviceId) return deviceId;
  try {
    const existing = localStorage.getItem(ANON_EPOCH_KEY);
    if (existing) return existing;
    const created = memoryAnonEpoch ?? `epoch_${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`;
    memoryAnonEpoch = created;
    try {
      localStorage.setItem(ANON_EPOCH_KEY, created);
    } catch {
      /* quota/private mode: the in-page memory epoch below still applies. */
    }
    return created;
  } catch {
    // Storage entirely unavailable: reuse the page-lifetime id.
    if (!memoryAnonEpoch) {
      memoryAnonEpoch = `epoch_mem_${Math.random().toString(36).slice(2)}`;
    }
    return memoryAnonEpoch;
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
