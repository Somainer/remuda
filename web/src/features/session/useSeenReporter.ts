import { useEffect, useRef } from "react";
import { hubStore } from "../../lib/store";
import type { Id, U64 } from "../../types/wire";

/**
 * How often a visible session page renews its Hub visibility lease. The Hub
 * holds one for 10 s, so a single lost renewal still leaves the lease alive.
 */
export const SEEN_RENEW_MS = 4_000;

const onScreen = () => document.visibilityState === "visible";

/**
 * seen-hub: while this session page is on screen, tell the Hub the owner is
 * looking at it. Each report marks the instance seen up to the row's durable
 * seq and renews this device's visibility lease, so a turn that ends while the
 * page is visible is folded seen by the Hub and never reads as ◉ on any owner
 * device. Hiding the tab, leaving the route, or unloading the page (close,
 * reload, full navigation: `pagehide`, sent keepalive) releases the lease; a
 * release that never arrives lets it lapse. Data only: renders nothing, and
 * every report is best-effort — a failure is retried by the next renewal.
 */
export function useSeenReporter(instanceId: Id | undefined, durableSeq: U64 | undefined, unseen: boolean) {
  const seq = useRef(durableSeq);
  const report = useRef<(visible: boolean) => void>(() => undefined);

  useEffect(() => {
    seq.current = durableSeq;
  }, [durableSeq]);

  useEffect(() => {
    if (!instanceId) return;
    const send = (visible: boolean) => {
      const durable = seq.current;
      if (durable == null) return;
      void hubStore.markSeen(instanceId, { durableSeq: durable, visible }).catch(() => undefined);
    };
    report.current = send;
    const renew = setInterval(() => {
      if (onScreen()) send(true);
    }, SEEN_RENEW_MS);
    const onVisibility = () => send(onScreen());
    const onPageHide = () => send(false);
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("pagehide", onPageHide);
    return () => {
      clearInterval(renew);
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("pagehide", onPageHide);
      report.current = () => undefined;
      send(false);
    };
  }, [instanceId]);

  // Report at once on opening the page, when the row arrives after a deep
  // link, and when a turn end the Hub could not fold (no lease yet, or a Hub
  // restart dropped it) shows up as unseen while the page is on screen.
  const known = durableSeq != null;
  useEffect(() => {
    if (known && onScreen()) report.current(true);
  }, [instanceId, known, unseen]);
}
