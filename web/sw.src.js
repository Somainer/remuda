// Service worker source. This file is NOT copied verbatim: the build plugin in
// vite.config.ts reads it, substitutes __CACHE_NAME__ with a per-build cache
// name (see cacheNameForBuild in src/lib/swCache.ts), and emits the result as
// dist/sw.js. Because the cache name carries the build identity the worker's
// bytes change on every deploy, so the browser's byte-compare update check
// sees a new worker and the activate sweep below reclaims the old shell.
const CACHE = "__CACHE_NAME__";
const SHELL = ["/", "/index.html", "/manifest.webmanifest", "/favicon.svg", "/icons/icon-192.png", "/icons/icon-512.png"];

self.addEventListener("install", (event) => {
  // No skipWaiting() here: a redeployed worker must sit in "waiting" while a
  // tab is driven by the old one, so the page can offer the "new version" bar
  // instead of being yanked mid-session. It takes over only when the client
  // posts ACTIVATE_UPDATE (the message handler below calls skipWaiting).
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
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
    // Network-first: fetch the document so a redeployed shell (new asset
    // hashes) is served immediately, refresh the cached copy on success, and
    // fall back to the cached shell only when the network fails.
    event.respondWith(
      (async () => {
        try {
          const fresh = await fetch(event.request);
          const cache = await caches.open(CACHE);
          await cache.put("/index.html", fresh.clone());
          return fresh;
        } catch {
          const cached = await caches.match("/index.html");
          return cached || Response.error();
        }
      })(),
    );
    return;
  }
  // Hashed /assets/* and other same-origin GETs stay cache-first: their names
  // already change per build, so a cached hit is always the right bytes.
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
