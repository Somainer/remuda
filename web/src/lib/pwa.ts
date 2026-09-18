import { useEffect, useState } from "react";

const INSTALL_DEMO_KEY = "remuda.install-banner";
const UPDATE_THROTTLE_MS = 60_000;

type BeforeInstall = Event & { prompt: () => Promise<void> };

export function isStandalone(): boolean {
  const nav = navigator as Navigator & { standalone?: boolean };
  return window.matchMedia("(display-mode: standalone)").matches || nav.standalone === true;
}

export function isIosDevice(): boolean {
  const ua = navigator.userAgent;
  if (/iPad|iPhone|iPod/.test(ua)) return true;
  return navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1;
}

export function installBannerRequested(): boolean {
  try {
    return sessionStorage.getItem(INSTALL_DEMO_KEY) === "1";
  } catch {
    return false;
  }
}

let lastUpdateCheck = 0;

export function checkForUpdate(): void {
  if (!("serviceWorker" in navigator)) return;
  const now = Date.now();
  if (now - lastUpdateCheck < UPDATE_THROTTLE_MS) return;
  lastUpdateCheck = now;
  void navigator.serviceWorker.getRegistration().then((reg) => reg?.update());
}

// A waiting worker means a redeployed shell is ready but the page is still
// driven by the old one. We surface a refresh affordance rather than reloading
// unannounced, then post ACTIVATE_UPDATE and reload once on controllerchange.
let updateWaiting: ServiceWorker | null = null;
const updateListeners = new Set<(available: boolean) => void>();
let reloading = false;

function announceUpdate(worker: ServiceWorker | null): void {
  updateWaiting = worker;
  for (const listener of updateListeners) listener(worker !== null);
}

export function subscribeUpdate(listener: (available: boolean) => void): () => void {
  updateListeners.add(listener);
  listener(updateWaiting !== null);
  return () => updateListeners.delete(listener);
}

/** Tell the waiting worker to take over; the controllerchange handler reloads. */
export function applyUpdate(): void {
  const worker = updateWaiting ?? navigator.serviceWorker?.controller;
  worker?.postMessage("ACTIVATE_UPDATE");
}

function watchRegistration(reg: ServiceWorkerRegistration): void {
  const check = (worker: ServiceWorker | null) => {
    // Only an update over an existing controller is a "new version"; the very
    // first install (no controller yet) is a fresh page, not an update.
    if (worker && worker.state === "installed" && navigator.serviceWorker.controller) {
      announceUpdate(worker);
    }
  };
  if (reg.waiting) check(reg.waiting);
  reg.addEventListener("updatefound", () => {
    const installing = reg.installing;
    if (!installing) return;
    installing.addEventListener("statechange", () => check(installing));
  });
}

export function startPWA(): void {
  if (window.isSecureContext && "serviceWorker" in navigator) {
    if (import.meta.env.PROD) {
      void navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" }).then((reg) => {
        watchRegistration(reg);
      });
    }
    // A single reload when the new worker takes control; the guard stops the
    // controllerchange → reload → controllerchange loop.
    navigator.serviceWorker.addEventListener("controllerchange", () => {
      if (reloading) return;
      reloading = true;
      window.location.reload();
    });
    navigator.serviceWorker.addEventListener("message", (event) => {
      const data = event.data as { type?: string; url?: string } | undefined;
      if (data?.type === "push-open" && typeof data.url === "string" && data.url.startsWith("/")) {
        window.location.assign(data.url);
      }
    });
  }
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") checkForUpdate();
  });
  window.addEventListener("focus", () => checkForUpdate());
}

export type InstallOffer =
  | { kind: "prompt"; prompt: () => Promise<void> }
  | { kind: "ios" }
  | { kind: "demo" };

/** True once a redeployed worker is waiting; drives the refresh bar. */
export function useUpdateAvailable(): boolean {
  const [available, setAvailable] = useState(false);
  useEffect(() => subscribeUpdate(setAvailable), []);
  return available;
}

export function useInstallPrompt(): InstallOffer | null {
  const [event, setEvent] = useState<BeforeInstall | null>(null);
  const [ios] = useState(() => typeof window !== "undefined" && !isStandalone() && isIosDevice());
  const [demo] = useState(() => typeof window !== "undefined" && !isStandalone() && installBannerRequested());

  useEffect(() => {
    if (isStandalone()) return;
    const onPrompt = (e: Event) => {
      e.preventDefault();
      setEvent(e as BeforeInstall);
    };
    window.addEventListener("beforeinstallprompt", onPrompt);
    return () => window.removeEventListener("beforeinstallprompt", onPrompt);
  }, []);

  if (event) return { kind: "prompt", prompt: () => event.prompt() };
  if (demo) return { kind: "demo" };
  if (ios) return { kind: "ios" };
  return null;
}
