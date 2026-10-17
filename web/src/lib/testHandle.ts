/**
 * Hub e2e test handle implementation (c-unboundresume r4 item 1).
 *
 * This module is the ONLY place that names `window.__remudaHub`. Store code
 * references this function exclusively behind the compile-time
 * {@link TEST_HOOKS_ENABLED} constant, so when that constant is `false`
 * (normal `vite build`) Rollup drops both the dead branch and this whole
 * side-effect-free module — the normal production artifact contains neither
 * the handle installation path nor the `__remudaHub` token. The dedicated
 * e2e artifact (`vite build --mode e2e` → web/dist-e2e) keeps it.
 */
import type { Id } from "../types/wire";

export type HubTestHandle = {
  stopPoll: () => void;
  settlementCount: () => number;
  interactionState: (id: Id) => string | undefined;
  waitFollowCaughtUp: (id: Id, timeoutMs?: number) => Promise<string>;
};

export function installHubTestHandle(handle: HubTestHandle): void {
  if (typeof window === "undefined") return;
  (window as unknown as { __remudaHub?: HubTestHandle }).__remudaHub = handle;
}

export function clearHubTestHandle(): void {
  if (typeof window === "undefined") return;
  delete (window as unknown as { __remudaHub?: unknown }).__remudaHub;
}
