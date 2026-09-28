// openpasture service worker. Two jobs: open the app when the phone has no signal (the app shell,
// kept from the last visit), and show push notifications. It never touches the API: /api, /mcp,
// /collar, /v1 and /hooks always go to the network, uncached, as if there were no worker. The
// farm's last state for offline use is the app's own copy (features/m/offline.ts), not a cache of
// /api answers. Other sites (map imagery) aren't touched either.

const SHELL = "openpasture-shell-v2";
const NEVER = /^\/(api|mcp|collar|v1|hooks)(\/|$)/;
// Files whose names change when their content does (the build's hashed assets): kept once fetched.
const FIXED = /^\/assets\//;
// Files kept under the same name when they change (fonts, icons, the manifest): the kept copy at
// once, and the network's for next time.
const NAMED = /^\/(fonts|icons)\/|^\/(favicon\.svg|manifest\.webmanifest)$/;
// A page the network hasn't answered in this long opens from the kept shell (one flaky bar hangs a
// request for minutes before it fails); the network's answer still refreshes the shell.
const NAV_WAIT_MS = 3000;

// What to do with a request: "pass" (the browser fetches it, the worker stays out), "page" (the app
// itself: the network, else the kept shell), "file" (kept after the first fetch) or "named" (the
// kept copy, refreshed from the network).
function route(url, method, mode, origin) {
  if (method !== "GET" || url.origin !== origin || NEVER.test(url.pathname)) return "pass";
  if (mode === "navigate") return "page";
  if (FIXED.test(url.pathname)) return "file";
  if (NAMED.test(url.pathname)) return "named";
  return "pass";
}

async function page(req) {
  const cache = await caches.open(SHELL);
  const net = fetch(req).then(async (res) => {
    if (res.ok) await cache.put("/", res.clone());
    return res;
  });
  net.catch(() => {});
  const kept = await cache.match("/");
  if (!kept) return net;
  const slow = new Promise((ok) => setTimeout(() => ok("slow"), NAV_WAIT_MS));
  try {
    const first = await Promise.race([net, slow]);
    return first === "slow" ? kept : first;
  } catch {
    return kept;
  }
}

async function file(req) {
  const cache = await caches.open(SHELL);
  const kept = await cache.match(req);
  if (kept) return kept;
  const res = await fetch(req);
  if (res.ok && res.type === "basic") await cache.put(req, res.clone());
  return res;
}

async function named(req) {
  const cache = await caches.open(SHELL);
  const fresh = fetch(req).then(async (res) => {
    if (res.ok && res.type === "basic") await cache.put(req, res.clone());
    return res;
  });
  const kept = await cache.match(req);
  if (!kept) return fresh;
  fresh.catch(() => {});
  return kept;
}

self.addEventListener("install", (e) => {
  self.skipWaiting();
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(["/", "/favicon.svg", "/manifest.webmanifest"])).catch(() => {}));
});

self.addEventListener("activate", (e) => {
  e.waitUntil((async () => {
    for (const k of await caches.keys()) if (k !== SHELL) await caches.delete(k);
    await self.clients.claim();
  })());
});

self.addEventListener("fetch", (e) => {
  const r = e.request;
  const kind = route(new URL(r.url), r.method, r.mode, self.location.origin);
  if (kind === "pass") return;
  e.respondWith(kind === "page" ? page(r) : kind === "named" ? named(r) : file(r));
});

// A notification: { title, body, tag, url, alert_id?, herd_id? } (op-alerts notify/push.rs).
self.addEventListener("push", (e) => {
  let n = {};
  try {
    n = e.data ? e.data.json() : {};
  } catch {
    n = { body: e.data ? e.data.text() : "" };
  }
  e.waitUntil(self.registration.showNotification(n.title || "openpasture", {
    body: n.body || "",
    tag: n.tag,
    renotify: !!n.tag,
    icon: "/icons/icon-192.png",
    badge: "/icons/badge-96.png",
    data: { url: n.url || "/#/map" },
  }));
});

// Tapping it: the open app goes there, else it opens there.
self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = new URL((e.notification.data && e.notification.data.url) || "/#/map", self.location.origin).href;
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const win = wins.find((w) => new URL(w.url).origin === self.location.origin);
    if (win) {
      await win.focus();
      win.postMessage({ type: "open", url });
      return;
    }
    await self.clients.openWindow(url);
  })());
});
