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

export function startPWA(): void {
  if (window.isSecureContext && "serviceWorker" in navigator) {
    if (import.meta.env.PROD) {
      void navigator.serviceWorker.register("/sw.js", { updateViaCache: "none" });
    }
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
