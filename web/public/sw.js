const CACHE = "runtime-shell-v2";
const SHELL = ["/", "/index.html", "/manifest.webmanifest", "/favicon.svg", "/icons/icon-192.png", "/icons/icon-512.png"];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const keys = await caches.keys();
      await Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("message", (event) => {
  if (event.data === "ACTIVATE_UPDATE") self.skipWaiting();
});

self.addEventListener("fetch", (event) => {
  if (event.request.method !== "GET") return;
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith("/v1/") || url.pathname.startsWith("/node/") || url.pathname.startsWith("/push/")) return;
  if (event.request.mode === "navigate") {
    event.respondWith(caches.match("/index.html").then((hit) => hit || fetch("/index.html")));
    return;
  }
  event.respondWith(caches.match(event.request).then((hit) => hit || fetch(event.request)));
});

function resolvePushDeepLink(payload) {
  const raw = payload && payload.data && typeof payload.data.url === "string" ? payload.data.url.trim() : "";
  if (raw.startsWith("/")) return raw;
  const tag = payload && typeof payload.tag === "string" ? payload.tag : "";
  if (tag.startsWith("interaction:")) return `/approvals?focus=${encodeURIComponent(tag.slice("interaction:".length))}`;
  if (tag.startsWith("instance:")) return `/s/${encodeURIComponent(tag.slice("instance:".length))}`;
  return "/sessions";
}

self.addEventListener("push", (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = {};
  }
  const tag = typeof payload.tag === "string" ? payload.tag : "";
  const title = payload.title || "runtime";
  const url = resolvePushDeepLink(payload);
  event.waitUntil(
    self.registration.showNotification(title, {
      body: payload.body || "",
      tag,
      renotify: Boolean(tag),
      data: { url },
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const path = (event.notification.data && event.notification.data.url) || "/sessions";
  const target = new URL(path, self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      for (const client of windows) {
        if (client.url.startsWith(self.location.origin) && "focus" in client) {
          client.postMessage({ type: "push-open", url: path });
          await client.focus();
          return;
        }
      }
      await self.clients.openWindow(target);
    })(),
  );
});
