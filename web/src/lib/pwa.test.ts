import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// startPWA/applyUpdate keep module-level state (was the page controlled at
// startup? has an update been accepted?), so every case gets a fresh module.
async function freshPwa() {
  vi.resetModules();
  return await import("./pwa");
}

function fakeWorker(state = "installed") {
  const worker = new EventTarget();
  Object.assign(worker, { state, postMessage: vi.fn() });
  return worker as EventTarget & { state: string; postMessage: ReturnType<typeof vi.fn> };
}

type FakeWorker = ReturnType<typeof fakeWorker>;

function fakeRegistration(controller: FakeWorker | null, waiting: FakeWorker | null) {
  const updatefound = new Set<() => void>();
  let installing: FakeWorker | null = null;
  return {
    controller,
    registration: {
      get waiting() {
        return waiting;
      },
      get installing() {
        return installing;
      },
      addEventListener(event: string, listener: () => void) {
        if (event === "updatefound") updatefound.add(listener);
      },
    },
    // Drive the lifecycle the browser would: updatefound, then installed.
    install(worker: FakeWorker) {
      installing = worker;
      worker.state = "installing";
      for (const listener of updatefound) listener();
      worker.state = "installed";
      worker.dispatchEvent(new Event("statechange"));
    },
  };
}

function stubServiceWorker(controller: FakeWorker | null, registration: unknown) {
  const container = new EventTarget();
  Object.assign(container, {
    controller,
    register: vi.fn(async () => registration),
    getRegistration: vi.fn(async () => registration),
  });
  Object.defineProperty(navigator, "serviceWorker", {
    configurable: true,
    value: container,
  });
  return container as EventTarget & { controller: FakeWorker | null; register: ReturnType<typeof vi.fn> };
}

describe("startPWA controllerchange scoping", () => {
  // jsdom's location.reload is a non-configurable property, so vi.spyOn cannot
  // wrap it; swap the whole location object for one with jest-style fns.
  const originalLocation = window.location;
  const reload = vi.fn();
  const assign = vi.fn();

  beforeEach(() => {
    Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...originalLocation, reload, assign },
    });
    reload.mockClear();
    assign.mockClear();
    vi.stubEnv("PROD", true);
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    Object.defineProperty(window, "location", { configurable: true, value: originalLocation });
  });

  it("does not reload when the first install claims an uncontrolled page", async () => {
    const pwa = await freshPwa();
    const first = fakeWorker("activated");
    const { registration, install } = fakeRegistration(null, null);
    const container = stubServiceWorker(null, registration);

    pwa.startPWA();
    await new Promise((resolve) => setTimeout(resolve, 0));
    // First-ever worker installs (no active predecessor, so it activates and
    // clients.claim() takes the tab).
    install(fakeWorker("installed"));
    container.controller = first;
    container.dispatchEvent(new Event("controllerchange"));

    expect(reload).not.toHaveBeenCalled();
    let available = true;
    pwa.subscribeUpdate((value) => {
      available = value;
    });
    expect(available).toBe(false);
  });

  it("reloads once when a page already controlled at startup gets a new controller", async () => {
    const pwa = await freshPwa();
    const active = fakeWorker("activated");
    const { registration } = fakeRegistration(active, null);
    const container = stubServiceWorker(active, registration);

    pwa.startPWA();
    container.dispatchEvent(new Event("controllerchange"));
    container.dispatchEvent(new Event("controllerchange"));

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("offers the bar for a waiting update and reloads only after it is accepted", async () => {
    const pwa = await freshPwa();
    const active = fakeWorker("activated");
    const setup = fakeRegistration(active, null);
    const container = stubServiceWorker(active, setup.registration);

    const seen: boolean[] = [];
    pwa.startPWA();
    await new Promise((resolve) => setTimeout(resolve, 0));
    pwa.subscribeUpdate((available) => seen.push(available));

    // v2 bytes arrive: updatefound then "installed" while v1 still controls.
    const next = fakeWorker("installing");
    setup.install(next);
    expect(seen).toContain(true);

    // A mere controllerchange before acceptance would be surprising; the bar
    // is the only path, and accepting posts ACTIVATE_UPDATE to the waiter.
    expect(next.postMessage).not.toHaveBeenCalled();
    pwa.applyUpdate();
    expect(next.postMessage).toHaveBeenCalledWith("ACTIVATE_UPDATE");
    expect(reload).not.toHaveBeenCalled();

    container.controller = next;
    container.dispatchEvent(new Event("controllerchange"));
    expect(reload).toHaveBeenCalledTimes(1);
  });
});
