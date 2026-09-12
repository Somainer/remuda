import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { notificationFromPayload, readPushStatus, subscribePush, unsubscribePush, urlBase64ToUint8Array } from "./push";
import { resolvePushDeepLink } from "./pushLink";

class FakeSubscription {
  endpoint = "https://push.example/sub/abc";
  expirationTime = null;
  options = { userVisibleOnly: true, applicationServerKey: null };
  onUnsubscribe?: () => void;
  toJSON() {
    return { endpoint: this.endpoint, keys: { p256dh: "cDI1NmRo", auth: "YXV0aHNlY3JldA" } };
  }
  getKey(): ArrayBuffer | null {
    return null;
  }
  unsubscribe = vi.fn(async () => {
    this.onUnsubscribe?.();
    return true;
  });
}

function stubNotification(permission: NotificationPermission = "default") {
  let current = permission;
  vi.stubGlobal(
    "Notification",
    class {
      static get permission() {
        return current;
      }
      static async requestPermission() {
        current = "granted";
        return current;
      }
    },
  );
}

function stubPushManager(existing: FakeSubscription | null = null) {
  let current = existing;
  if (current) current.onUnsubscribe = () => {
    current = null;
  };
  const registration = {
    pushManager: {
      getSubscription: async () => current,
      subscribe: async () => {
        current = new FakeSubscription();
        current.onUnsubscribe = () => {
          current = null;
        };
        return current;
      },
    },
  };
  Object.defineProperty(navigator, "serviceWorker", {
    configurable: true,
    value: {
      getRegistration: async () => registration,
      register: async () => registration,
      ready: Promise.resolve(registration),
      addEventListener() {},
    },
  });
  return registration;
}

describe("push deep links", () => {
  it("prefers data.url, else tag, else sessions", () => {
    expect(resolvePushDeepLink({ data: { url: "/s/ins_1" } })).toBe("/s/ins_1");
    expect(resolvePushDeepLink({ tag: "interaction:int_9" })).toBe("/approvals?focus=int_9");
    expect(resolvePushDeepLink({ tag: "instance:ins_2" })).toBe("/s/ins_2");
    expect(resolvePushDeepLink({})).toBe("/sessions");
  });

  it("passes tag through so the browser can collapse duplicates", () => {
    const shown = notificationFromPayload({
      title: "审批",
      body: "Bash",
      tag: "interaction:int_9",
      data: { url: "/approvals?focus=int_9" },
    });
    expect(shown.title).toBe("审批");
    expect(shown.options.tag).toBe("interaction:int_9");
    expect((shown.options.data as { url: string }).url).toBe("/approvals?focus=int_9");
  });
});

describe("urlBase64ToUint8Array", () => {
  it("decodes url-safe VAPID material", () => {
    const bytes = urlBase64ToUint8Array("AQID");
    expect(Array.from(bytes)).toEqual([1, 2, 3]);
  });
});

describe("subscribePush / unsubscribePush with mocked PushManager", () => {
  const posted: unknown[] = [];
  const deleted: string[] = [];

  beforeEach(() => {
    posted.length = 0;
    deleted.length = 0;
    localStorage.clear();
    stubNotification("default");
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.includes("/push/config")) {
          return new Response(JSON.stringify({ public_key: "AQID" }), { status: 200 });
        }
        if (url.includes("/push/subscriptions") && (init?.method ?? "GET") === "POST") {
          posted.push(JSON.parse(String(init?.body)));
          return new Response(JSON.stringify({ ok: true }), { status: 201 });
        }
        if (url.includes("/push/subscriptions") && init?.method === "DELETE") {
          deleted.push(url);
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        return new Response("missing", { status: 404 });
      }),
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    localStorage.clear();
  });

  it("GETs VAPID config, subscribes, and POSTs endpoint keys + deviceId", async () => {
    stubPushManager();
    localStorage.setItem("runtime.device-settings.v1", JSON.stringify({ deviceName: "phone" }));
    const result = await subscribePush();
    expect(result.ok).toBe(true);
    expect(result.endpoint).toBe("https://push.example/sub/abc");
    expect(posted).toEqual([
      {
        endpoint: "https://push.example/sub/abc",
        keys: { p256dh: "cDI1NmRo", auth: "YXV0aHNlY3JldA" },
        deviceId: "phone",
      },
    ]);
    const status = await readPushStatus();
    expect(status.subscribed).toBe(true);
  });

  it("unsubscribes the PushSubscription and DELETE /push/subscriptions", async () => {
    const sub = new FakeSubscription();
    stubPushManager(sub);
    localStorage.setItem("runtime.push-endpoint.v1", sub.endpoint);
    const result = await unsubscribePush();
    expect(result.ok).toBe(true);
    expect(sub.unsubscribe).toHaveBeenCalled();
    expect(deleted[0]).toContain("endpoint=" + encodeURIComponent(sub.endpoint));
    const status = await readPushStatus();
    expect(status.subscribed).toBe(false);
  });

  it("refuses iOS Safari that is not on the home screen", async () => {
    Object.defineProperty(navigator, "userAgent", { configurable: true, value: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)" });
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    });
    const result = await subscribePush();
    expect(result.ok).toBe(false);
    expect(result.reason).toBe("ios-homescreen");
    expect(posted).toHaveLength(0);
  });
});
