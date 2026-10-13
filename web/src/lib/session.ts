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
  // Logout/reinit ROTATES the auth epoch (c-reconnfu gate 30 item 1): drop the
  // page-lifetime cached id and publish an empty TOMBSTONE to the epoch key
  // (c-reconnfu gate 31 item 3) instead of deleting it. Other tabs hold the
  // old id in their own page caches; the tombstone is observable (read-before-
  // use + the storage event), so they drop their cache and mint a fresh epoch
  // rather than republishing the deleted one. A device-session epoch is gone
  // with the session itself.
  memoryAnonEpoch = null;
  try {
    localStorage.removeItem(LEGACY_ACCESS_KEY);
    localStorage.removeItem(SESSION_KEY);
    localStorage.setItem(ANON_EPOCH_KEY, "");
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

function mintAnonEpoch(): string {
  return `epoch_${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`;
}

// Observe OTHER documents rotating the epoch (logout tombstone or a freshly
// minted id): keep this page's cache in lock-step with storage so it can never
// republish an obsolete id. Read-before-use in readAuthEpoch is the primary
// guard; this keeps the cached fallback (storage errors) honest too.
if (typeof window !== "undefined" && typeof window.addEventListener === "function") {
  window.addEventListener("storage", (event: StorageEvent) => {
    if (event.key !== ANON_EPOCH_KEY) return;
    // newValue null = key removed; "" = explicit logout tombstone — either way
    // the cached id is obsolete.
    memoryAnonEpoch = event.newValue ? event.newValue : null;
  });
}

/**
 * The current AUTH EPOCH that owns outbox deliveries (c-reconnfu gate 28):
 * normally the logged-in device session id (stable across reloads of the same
 * login, changed by logout/reinit). When no device session exists (mock/test
 * harness without a login), fall back to a stable id persisted in
 * localStorage so a reload keeps the same epoch while a logout (which writes
 * a tombstone via clearSession) starts a new one.
 *
 * Cross-tab rule (c-reconnfu gate 31 item 3): storage is ALWAYS read before
 * the page cache is used. The cached id is valid only while the stored id
 * equals it; a tombstone/rotation observed from another tab drops the cache
 * and mints fresh, and an id not confirmed current in storage is never
 * written back.
 */
export function readAuthEpoch(): string {
  const deviceId = readSession()?.deviceId;
  if (deviceId) return deviceId;
  try {
    const stored = localStorage.getItem(ANON_EPOCH_KEY);
    // A non-empty stored value is the shared current epoch — sync the page
    // cache to it (covers another tab's rotation) and return it.
    if (stored) {
      memoryAnonEpoch = stored;
      return stored;
    }
    if (stored === "") {
      // Explicit logout TOMBSTONE: the old id (cached or not) is revoked.
      // Mint fresh — never reuse — and publish the new id.
      const rotated = mintAnonEpoch();
      memoryAnonEpoch = rotated;
      try {
        localStorage.setItem(ANON_EPOCH_KEY, rotated);
      } catch {
        /* quota/private mode: the page-lifetime id still applies. */
      }
      return rotated;
    }
    // stored === null (first run, or an earlier write that failed): a cached
    // id is OUR OWN unconfirmed-but-current page epoch. With the empty-string
    // tombstone convention, a logged-out state is never null, so re-asserting
    // the cached id here cannot resurrect a revoked epoch — write it back
    // (best effort) and reuse it; a fresh page with no cache mints one.
    const created = memoryAnonEpoch ?? mintAnonEpoch();
    memoryAnonEpoch = created;
    try {
      localStorage.setItem(ANON_EPOCH_KEY, created);
    } catch {
      /* quota/private mode: the in-page memory epoch below still applies. */
    }
    return created;
  } catch {
    // Storage entirely unavailable: reuse the page-lifetime id WITHOUT writing
    // it back (it cannot be confirmed current across tabs).
    if (!memoryAnonEpoch) {
      memoryAnonEpoch = mintAnonEpoch();
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
