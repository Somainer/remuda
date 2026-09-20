import { deletePushSubscription, fetchPushConfig, postPushSubscription } from "./api";
import { isIosDevice, isStandalone } from "./pwa";
import { readDeviceSettings } from "../features/settings";
import { hubStore } from "./store";
import { resolvePushDeepLink } from "./pushLink";

const ENDPOINT_KEY = "runtime.push-endpoint.v1";

export type PushReason = "ok" | "denied" | "ios-homescreen" | "unsupported" | "error";

export type PushResult = { ok: boolean; reason: PushReason; endpoint?: string };

export type PushStatus = {
  permission: NotificationPermission | "unsupported";
  subscribed: boolean;
  endpoint: string | null;
  needsHomeScreen: boolean;
};

/** The Badging API (https://w3c.github.io/badging/), present only on some
 *  platforms (installed Chromium PWA, Android). Safari/iOS lacks it. */
type NavigatorBadging = Navigator & {
  setAppBadge?: (contents?: number) => Promise<void>;
  clearAppBadge?: () => Promise<void>;
};

export function needsHomeScreenForNotifications(): boolean {
  return isIosDevice() && !isStandalone();
}

export function urlBase64ToUint8Array(base64: string): Uint8Array {
  const padded = base64.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (base64.length % 4)) % 4);
  const raw = atob(padded);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i += 1) out[i] = raw.charCodeAt(i);
  return out;
}

function rememberEndpoint(endpoint: string | null): void {
  try {
    if (!endpoint) localStorage.removeItem(ENDPOINT_KEY);
    else localStorage.setItem(ENDPOINT_KEY, endpoint);
  } catch {
    /* ignore */
  }
}

function rememberedEndpoint(): string | null {
  try {
    return localStorage.getItem(ENDPOINT_KEY);
  } catch {
    return null;
  }
}

export type PushNotification = {
  title: string;
  options: NotificationOptions;
  /**
   * The Hub-stamped pending interaction count from the push payload
   * (D-049 ui-spec §4.5). Absent for old-Hub payloads; never synthesized
   * from a notification count.
   */
  badge?: number;
};

export function notificationFromPayload(raw: unknown): PushNotification {
  const body = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
  const tag = typeof body.tag === "string" ? body.tag : "";
  const data = body.data && typeof body.data === "object" ? (body.data as Record<string, unknown>) : {};
  const url = resolvePushDeepLink({
    tag,
    data: { url: typeof data.url === "string" ? data.url : undefined },
  });
  const notice: PushNotification = {
    title: typeof body.title === "string" && body.title ? body.title : "runtime",
    options: {
      body: typeof body.body === "string" ? body.body : "",
      tag,
      data: { url },
      ...(tag ? { renotify: true } : {}),
    } as NotificationOptions,
  };
  // Optional integer only: missing / wrong-typed / negative payloads stay
  // badge-less, which is exactly the old-client/old-Hub case.
  if (typeof body.badge === "number" && Number.isInteger(body.badge) && body.badge >= 0) {
    notice.badge = body.badge;
  }
  return notice;
}

/**
 * Set the OS app badge to the device's pending interaction count, clearing
 * at zero (D-049 ui-spec §4.5). Driven from the page while the PWA is open;
 * the closed-state push path is the service worker's job. Honest
 * degradation: where the Badging API does not exist this is a complete
 * no-op — the notification count is never substituted. The badge is
 * cosmetic, so a rejected promise is swallowed.
 */
export function syncAppBadge(pending: number): void {
  const nav = navigator as NavigatorBadging;
  if (pending > 0) {
    if (typeof nav.setAppBadge !== "function") return;
    void nav.setAppBadge(pending).catch(() => undefined);
  } else if (typeof nav.clearAppBadge === "function") {
    void nav.clearAppBadge().catch(() => undefined);
  } else if (typeof nav.setAppBadge === "function") {
    // setAppBadge(0) clears on platforms that ship the setter without the
    // named clearer.
    void nav.setAppBadge(0).catch(() => undefined);
  }
}

/** Pending interactions, same rule the in-app badges use (Shell/PhoneShell). */
export function pendingInteractionCount(): number {
  return hubStore.getSnapshot().interactions.filter((item) => item.state === "pending").length;
}

let badgeUnsubscribe: (() => void) | null = null;
let lastBadgeCount: number | null = null;

/**
 * Keep the OS badge in sync with the pending interaction count for the
 * lifetime of the page. Idempotent: mounting twice (React StrictMode) keeps
 * a single subscription. Returns the stop handle.
 */
export function startAppBadgeSync(): () => void {
  if (!badgeUnsubscribe) {
    const apply = () => {
      const count = pendingInteractionCount();
      if (count !== lastBadgeCount) {
        lastBadgeCount = count;
        syncAppBadge(count);
      }
    };
    apply();
    badgeUnsubscribe = hubStore.subscribe(apply);
  }
  return stopAppBadgeSync;
}

/** Test/lifecycle teardown: detach and forget the last synced count. */
export function stopAppBadgeSync(): void {
  badgeUnsubscribe?.();
  badgeUnsubscribe = null;
  lastBadgeCount = null;
}

async function ensureRegistration(): Promise<ServiceWorkerRegistration | null> {
  if (!("serviceWorker" in navigator)) return null;
  const existing = await navigator.serviceWorker.getRegistration();
  if (existing) return existing;
  try {
    return await navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" });
  } catch {
    return navigator.serviceWorker.ready.catch(() => null);
  }
}

export async function readPushStatus(): Promise<PushStatus> {
  const needsHomeScreen = needsHomeScreenForNotifications();
  if (typeof Notification === "undefined") {
    return { permission: "unsupported", subscribed: false, endpoint: rememberedEndpoint(), needsHomeScreen };
  }
  let endpoint: string | null = rememberedEndpoint();
  let subscribed = Boolean(endpoint);
  if ("serviceWorker" in navigator) {
    try {
      const reg = await navigator.serviceWorker.getRegistration();
      const sub = await reg?.pushManager.getSubscription();
      if (sub) {
        endpoint = sub.endpoint;
        subscribed = true;
      } else {
        subscribed = false;
        endpoint = null;
      }
    } catch {
      /* keep remembered */
    }
  }
  return {
    permission: Notification.permission,
    subscribed,
    endpoint,
    needsHomeScreen,
  };
}

export async function subscribePush(): Promise<PushResult> {
  if (needsHomeScreenForNotifications()) return { ok: false, reason: "ios-homescreen" };
  if (typeof Notification === "undefined" || !("serviceWorker" in navigator)) {
    return { ok: false, reason: "unsupported" };
  }
  try {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") return { ok: false, reason: "denied" };
    const config = await fetchPushConfig();
    const registration = await ensureRegistration();
    if (!registration?.pushManager) return { ok: false, reason: "unsupported" };
    const existing = await registration.pushManager.getSubscription();
    const sub =
      existing ??
      (await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToUint8Array(config.public_key) as BufferSource,
      }));
    const json = sub.toJSON();
    const endpoint = json.endpoint ?? sub.endpoint;
    const p256dh = json.keys?.p256dh;
    const auth = json.keys?.auth;
    if (!endpoint || !p256dh || !auth) return { ok: false, reason: "error" };
    const deviceId = readDeviceSettings().deviceName || "device";
    await postPushSubscription({ endpoint, keys: { p256dh, auth }, deviceId });
    rememberEndpoint(endpoint);
    return { ok: true, reason: "ok", endpoint };
  } catch {
    return { ok: false, reason: "error" };
  }
}

export async function unsubscribePush(): Promise<PushResult> {
  try {
    let endpoint = rememberedEndpoint();
    if ("serviceWorker" in navigator) {
      const reg = await navigator.serviceWorker.getRegistration();
      const sub = await reg?.pushManager.getSubscription();
      if (sub) {
        endpoint = sub.endpoint;
        await sub.unsubscribe();
      }
    }
    if (endpoint) await deletePushSubscription(endpoint);
    rememberEndpoint(null);
    return { ok: true, reason: "ok", endpoint: endpoint ?? undefined };
  } catch {
    return { ok: false, reason: "error" };
  }
}
