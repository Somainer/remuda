// Service worker source. This file is NOT copied verbatim: the build plugin
// (sw-build.ts, wired from vite.config.ts) stamps two placeholder tokens:
//   * on the const CACHE line — a per-build cache name (see
//     cacheNameForBuild in src/lib/swCache.ts),
//   * on the const PRECACHE_URLS line — the build-derived precache manifest:
//     the index.html entry closure plus EVERY lazy route chunk and their
//     imported CSS/assets, so an offline first visit to a never-visited route
//     loads. The list comes from the Vite build graph at build time and is
//     never hand-maintained.
// The result is emitted as dist/sw.js. Because the cache name (and the
// hashed asset list) carries the build identity the worker's bytes change on
// every deploy, so the browser's byte-compare update check sees a new worker
// and the activate sweep below reclaims the old shell. Old builds keep their
// OWN cache until the replacement worker activates (no skipWaiting): a tab
// still running the old build keeps every hashed asset it needs.
const CACHE = "runtime-shell-1f2d2265f6fa";
const PRECACHE_URLS = ["/assets/ApprovalCard-C2zShf-I.js","/assets/ApprovalCard-DdwQGKIQ.css","/assets/ApprovalsPage-CQy3tbes.js","/assets/Board-Bk_Bcdzy.js","/assets/Board-C5_OI3UH.css","/assets/BotsPage-BgXbIJT4.css","/assets/BotsPage-D9qbSCmx.js","/assets/Button-DWfmWTHu.js","/assets/CommitProbe-Bbl9bvHY.js","/assets/EffortSlider-CWOiD9nt.js","/assets/EffortSlider-CfDaoh9y.css","/assets/FleetPage-CcgC89eq.css","/assets/FleetPage-wm5w7zJe.js","/assets/HomeList-BBGhEF6x.js","/assets/HomeList-BCNxIqCG.css","/assets/HostsPage-D4yKgYVi.js","/assets/Inbox-B9J6X-B5.js","/assets/InboxShell-BFqhgYVx.css","/assets/InboxShell-U8hIjCir.js","/assets/KaTeX_AMS-Regular-BQhdFMY1.woff2","/assets/KaTeX_Caligraphic-Bold-Dq_IR9rO.woff2","/assets/KaTeX_Caligraphic-Regular-Di6jR-x-.woff2","/assets/KaTeX_Fraktur-Bold-CL6g_b3V.woff2","/assets/KaTeX_Fraktur-Regular-CTYiF6lA.woff2","/assets/KaTeX_Main-Bold-Cx986IdX.woff2","/assets/KaTeX_Main-BoldItalic-DxDJ3AOS.woff2","/assets/KaTeX_Main-Italic-NWA7e6Wa.woff2","/assets/KaTeX_Main-Regular-B22Nviop.woff2","/assets/KaTeX_Math-BoldItalic-CZnvNsCZ.woff2","/assets/KaTeX_Math-Italic-t53AETM-.woff2","/assets/KaTeX_SansSerif-Bold-D1sUS0GD.woff2","/assets/KaTeX_SansSerif-Italic-C3H0VqGB.woff2","/assets/KaTeX_SansSerif-Regular-DDBCnlJ7.woff2","/assets/KaTeX_Script-Regular-D3wIWfF6.woff2","/assets/KaTeX_Size1-Regular-mCD8mA8B.woff2","/assets/KaTeX_Size2-Regular-Dy4dx90m.woff2","/assets/KaTeX_Size4-Regular-Dl5lxZxV.woff2","/assets/KaTeX_Typewriter-Regular-CO6r4hn1.woff2","/assets/Modal-BrFRpY_d.js","/assets/NewSessionPage-BwZs9mI_.css","/assets/NewSessionPage-By057sCK.js","/assets/ProjectsPage-DYT7cgX3.css","/assets/ProjectsPage-Dx93UhJX.js","/assets/ProvidersPage-Dj_dXL4N.js","/assets/SessionPage-BPLLPf8C.css","/assets/SessionPage-D8q_GKr2.js","/assets/SessionsPage-0KFPHKl1.js","/assets/SessionsPage-DxWmViIi.css","/assets/SettingsPage-D5A98pA_.css","/assets/SettingsPage-FSS8xyEK.js","/assets/SubagentView-CcKAjCWl.js","/assets/TaskList-BYZAR5p8.css","/assets/TaskList-CgMge4JH.js","/assets/ToolCard-B-fnuPpo.css","/assets/ToolCard-Bd9k77TD.js","/assets/accessCode-lGGhgB8H.js","/assets/addon-canvas-CY8shz_h.js","/assets/addon-webgl-BQImtbV0.js","/assets/api-gRM4zWuA.js","/assets/bash-LmRjUKlR.js","/assets/clipboard-CMJanXyi.js","/assets/command-BLygjlZi.js","/assets/commandStatus-C2x5F0mD.js","/assets/core-VbNX68N4.js","/assets/endReason-CeRYjaHo.js","/assets/hosts-BWmqCGfv.css","/assets/hosts-cI22l-ju.js","/assets/ibm-plex-mono-latin-400-normal-CvHOgSBP.woff","/assets/ibm-plex-mono-latin-400-normal-DMJ8VG8y.woff2","/assets/ibm-plex-mono-latin-500-normal-CB9ihrfo.woff","/assets/ibm-plex-mono-latin-500-normal-DSY6xOcd.woff2","/assets/index-BSJcWH2n.css","/assets/index-DgHgGWjk.js","/assets/javascript-8AmHuH5O.js","/assets/json-PmyKWHmm.js","/assets/jsx-runtime-RRncGqZB.js","/assets/katex-BHxplFoT.js","/assets/mathKatex-B2XouDXw.css","/assets/nextStep-DIKv3CFP.js","/assets/overlay-DtsqXxcV.css","/assets/overlay.module-DY3FYAZ7.js","/assets/providers-BOSDV_ci.js","/assets/providers-yfboBDIV.css","/assets/python-gHmu6VPG.js","/assets/registry-CG29PxS0.js","/assets/rolldown-runtime-hePW80VL.js","/assets/rust-BIKPVoi6.js","/assets/sessionOptions-6-pXTb3k.js","/assets/status-DjBFJzNw.js","/assets/store-CERgR8pi.js","/assets/typescript-CxkPLmSc.js","/assets/ui-RsYJQwb-.css","/assets/ui.module-ByQNywmk.js","/assets/useNowTick-Gyk__l_V.js","/assets/workspaces-DyX_ahjC.css","/assets/workspaces.module-CwBJeH1J.js"];
const SHELL = ["/", "/index.html", "/manifest.webmanifest", "/favicon.svg", "/icons/icon-192.png", "/icons/icon-512.png"];

self.addEventListener("install", (event) => {
  // No skipWaiting() here: a redeployed worker must sit in "waiting" while a
  // tab is driven by the old one, so the page can offer the "new version" bar
  // instead of being yanked mid-session. It takes over only when the client
  // posts ACTIVATE_UPDATE (the message handler below calls skipWaiting).
  // addAll is atomic: every route chunk and import must exist or the worker
  // fails install rather than taking control with a hole in the precache.
  event.waitUntil(
    caches.open(CACHE).then((cache) => cache.addAll(SHELL.concat(PRECACHE_URLS))),
  );
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
    // hashes) is served immediately, and fall back to the cached shell only
    // when the network fails.
    event.respondWith(
      (async () => {
        try {
          const fresh = await fetch(event.request);
          // Refresh the cached shell only from a good document. The clone must
          // be taken NOW, before fresh is returned to respondWith: afterwards
          // the browser locks the body to stream the document, and a clone()
          // deferred into the caches.open callback throws (the .catch below
          // would silently freeze the cached shell on the old bytes). A 502
          // must also never become the offline shell, and a write that rejects
          // (quota, a 206, a storage error) stays inside waitUntil rather than
          // discarding the document the network just delivered.
          if (fresh.ok) {
            const copy = fresh.clone();
            event.waitUntil(
              caches
                .open(CACHE)
                .then((cache) => cache.put("/index.html", copy))
                .catch(() => undefined),
            );
          }
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
