/**
 * Dismissed in-app notifications, scoped per session.
 *
 * The list itself is derived from the journal (durable); only the *dismissed*
 * set is client state, held for the page's life. A module store with
 * `useSyncExternalStore` keeps one source per mounted session page. Snapshots
 * are cached per instance and rebuilt only on a change (the store contract
 * requires `getSnapshot` to return a referentially-stable value).
 */
import { useSyncExternalStore } from "react";

const dismissed = new Set<string>();
const listeners = new Set<() => void>();
const cache = new Map<string, ReadonlySet<string>>();

function key(instanceId: string, id: string): string {
  return `${instanceId}:${id}`;
}

function rebuild(instanceId: string): ReadonlySet<string> {
  const out = new Set<string>();
  const prefix = `${instanceId}:`;
  for (const k of dismissed) if (k.startsWith(prefix)) out.add(k.slice(prefix.length));
  cache.set(instanceId, out);
  return out;
}

function emit() {
  cache.clear();
  for (const listener of listeners) listener();
}

export function dismissNotification(instanceId: string, id: string): void {
  dismissed.add(key(instanceId, id));
  emit();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useDismissedNotifications(instanceId: string): ReadonlySet<string> {
  return useSyncExternalStore(
    subscribe,
    () => cache.get(instanceId) ?? rebuild(instanceId),
    () => cache.get(instanceId) ?? rebuild(instanceId),
  );
}

/** Test-only isolation: forget every dismissal. */
export function __resetDismissedForTests(): void {
  dismissed.clear();
  cache.clear();
  emit();
}
