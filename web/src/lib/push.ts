/** Web Push subscription shape (protocol/ui-spec). Mock no-ops without VAPID. */

export async function subscribePush(): Promise<{ ok: boolean }> {
  if (!("Notification" in window) || !("serviceWorker" in navigator)) return { ok: false };
  const permission = await Notification.requestPermission();
  return { ok: permission === "granted" };
}
