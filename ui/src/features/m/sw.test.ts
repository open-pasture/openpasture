// ui/public/sw.js, run in a service worker's global as the browser would give it: its own
// CacheStorage and fetch (the browser's parts, not the app's). The worker must never answer or
// keep anything from the API.

import { beforeEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const SRC = readFileSync(join(import.meta.dir, "../../../public/sw.js"), "utf8");
const ORIGIN = "https://farm.example.com";

class FakeCache {
  entries = new Map<string, Response>();
  key = (r: Request | string) => (typeof r === "string" ? new URL(r, ORIGIN).href : r.url);
  async match(r: Request | string) {
    return this.entries.get(this.key(r))?.clone();
  }
  async put(r: Request | string, res: Response) {
    this.entries.set(this.key(r), res);
  }
  async addAll(urls: string[]) {
    for (const u of urls) this.entries.set(this.key(u), new Response(`kept ${u}`));
  }
}

let caches: Map<string, FakeCache>;
let listeners: Record<string, (e: unknown) => void>;
let net: { online: boolean; hang: boolean; version: number; seen: string[] };
let shown: { title: string; opts: NotificationOptions }[];
let windows: { url: string; focused: boolean; messages: unknown[] }[];
let opened: string[];
let route: (url: URL, method: string, mode: string, origin: string) => string;

beforeEach(() => {
  caches = new Map();
  listeners = {};
  net = { online: true, hang: false, version: 0, seen: [] };
  shown = [];
  windows = [];
  opened = [];
  const storage = {
    open: async (n: string) => caches.get(n) ?? (caches.set(n, new FakeCache()), caches.get(n)!),
    keys: async () => [...caches.keys()],
    delete: async (n: string) => caches.delete(n),
    match: async (r: Request | string) => {
      for (const c of caches.values()) {
        const hit = await c.match(r);
        if (hit) return hit;
      }
    },
  };
  const fetchFn = async (r: Request) => {
    net.seen.push(r.url);
    // One flaky bar: the request neither answers nor fails.
    if (net.hang) return new Promise<Response>(() => {});
    if (!net.online) throw new TypeError("Failed to fetch");
    return Object.defineProperty(new Response(`net ${r.url}${net.version ? ` v${net.version}` : ""}`), "type", { value: "basic" });
  };
  // The worker's timers run a thousand times faster here.
  const timer = (f: () => void, ms: number) => setTimeout(f, ms / 1000);
  const self = {
    location: { origin: ORIGIN },
    addEventListener: (t: string, f: (e: unknown) => void) => (listeners[t] = f),
    skipWaiting: () => {},
    clients: {
      claim: async () => {},
      matchAll: async () => windows.map((w) => ({ url: w.url, focus: async () => (w.focused = true), postMessage: (m: unknown) => w.messages.push(m) })),
      openWindow: async (u: string) => void opened.push(u),
    },
    registration: { showNotification: async (title: string, opts: NotificationOptions) => void shown.push({ title, opts }) },
  };
  route = new Function("self", "caches", "fetch", "setTimeout", `${SRC}\nreturn route;`)(self, storage, fetchFn, timer);
});

// A fetch event; returns the response the worker gave, or "passed" when it stayed out.
async function request(path: string, init: { method?: string; mode?: RequestMode; origin?: string } = {}) {
  const url = new URL(path, init.origin ?? ORIGIN).href;
  const req = new Request(url, { method: init.method ?? "GET" });
  Object.defineProperty(req, "mode", { value: init.mode ?? "cors" });
  let answer: Promise<Response> | undefined;
  listeners.fetch({ request: req, respondWith: (p: Promise<Response>) => (answer = p) });
  return answer ? await answer : "passed";
}

const kept = () => [...caches.values()].flatMap((c) => [...c.entries.keys()]);

const API = [
  ["/api/state", "GET"], ["/api/live", "GET"], ["/api/herds/h1/boundary", "POST"], ["/api/alerts/a1/ack", "POST"], ["/api/push", "GET"],
  ["/api/push/subscriptions", "POST"], ["/api/collars?herd_id=h1", "GET"], ["/api", "GET"], ["/mcp", "POST"], ["/collar/v1/report", "POST"],
  ["/v1/ask", "POST"], ["/v1/notify/inbox?since=0", "GET"], ["/hooks/twilio/sms", "POST"],
] as const;

describe("the service worker", () => {
  test("stays out of every API request, even one opened as a page", async () => {
    for (const [path, method] of API) {
      expect(route(new URL(path, ORIGIN), method, "cors", ORIGIN)).toBe("pass");
      expect(route(new URL(path, ORIGIN), method, "navigate", ORIGIN)).toBe("pass");
      expect(await request(path, { method })).toBe("passed");
      expect(await request(path, { method, mode: "navigate" })).toBe("passed");
    }
    expect(net.seen).toEqual([]);
    expect(kept()).toEqual([]);
  });

  test("keeps the app shell and its files, and nothing from the API", async () => {
    listeners.install({ waitUntil: (p: Promise<unknown>) => p });
    await new Promise((r) => setTimeout(r, 0));
    for (const [path, method] of API) await request(path, { method });
    const page = await request("/", { mode: "navigate" });
    expect(await (page as Response).text()).toBe(`net ${ORIGIN}/`);
    await request("/assets/index-Ab12.js");
    await request("/fonts/inter-var.woff2");
    await request("/icons/icon-192.png");
    expect(kept().sort()).toEqual([`${ORIGIN}/`, `${ORIGIN}/assets/index-Ab12.js`, `${ORIGIN}/favicon.svg`, `${ORIGIN}/fonts/inter-var.woff2`, `${ORIGIN}/icons/icon-192.png`, `${ORIGIN}/manifest.webmanifest`]);
    expect(kept().some((k) => /\/(api|mcp|collar|v1|hooks)(\/|$|\?)/.test(new URL(k).pathname))).toBe(false);
  });

  test("opens the kept shell without signal, and files it has kept", async () => {
    await request("/", { mode: "navigate" });
    await request("/assets/index-Ab12.js");
    net.online = false;
    expect(await ((await request("/", { mode: "navigate" })) as Response).text()).toBe(`net ${ORIGIN}/`);
    expect(await ((await request("/assets/index-Ab12.js")) as Response).text()).toBe(`net ${ORIGIN}/assets/index-Ab12.js`);
    // The API fails as it would with no worker: the app shows its own last copy.
    expect(await request("/api/state")).toBe("passed");
  });

  test("opens the kept shell when the network hangs, not a blank page", async () => {
    await request("/", { mode: "navigate" });
    net.hang = true;
    const page = await request("/", { mode: "navigate" });
    expect(await (page as Response).text()).toBe(`net ${ORIGIN}/`);
  });

  test("files kept under the same name are refreshed; hashed assets are kept as they are", async () => {
    for (const p of ["/icons/icon-192.png", "/fonts/inter-var.woff2", "/manifest.webmanifest", "/favicon.svg", "/assets/index-Ab12.js"]) await request(p);
    net.version = 2;
    for (const p of ["/icons/icon-192.png", "/manifest.webmanifest"]) {
      // The kept copy at once, the new one fetched for next time.
      expect(await ((await request(p)) as Response).text()).toBe(`net ${ORIGIN}${p}`);
      await new Promise((r) => setTimeout(r, 0));
      expect(await ((await request(p)) as Response).text()).toBe(`net ${ORIGIN}${p} v2`);
    }
    expect(await ((await request("/assets/index-Ab12.js")) as Response).text()).toBe(`net ${ORIGIN}/assets/index-Ab12.js`);
    net.online = false;
    expect(await ((await request("/icons/icon-192.png")) as Response).text()).toBe(`net ${ORIGIN}/icons/icon-192.png v2`);
  });

  test("leaves other sites (imagery) and other files to the browser", async () => {
    expect(await request("https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/16/1/2", { origin: "https://server.arcgisonline.com" })).toBe("passed");
    expect(await request("/sw.js")).toBe("passed");
    expect(await request("/assets/x.js", { method: "POST" })).toBe("passed");
    expect(kept()).toEqual([]);
  });

  test("shows a push as a notification that opens its alert", async () => {
    const payload = { title: "Test farm", body: "214 outside P1, 6m", tag: "alr_1", url: "/#/map?alert=alr_1", alert_id: "alr_1" };
    let done: Promise<unknown> = Promise.resolve();
    listeners.push({ data: { json: () => payload, text: () => JSON.stringify(payload) }, waitUntil: (p: Promise<unknown>) => (done = p) });
    await done;
    expect(shown).toHaveLength(1);
    expect(shown[0].title).toBe("Test farm");
    expect([shown[0].opts.body, shown[0].opts.tag, (shown[0].opts.data as { url: string }).url]).toEqual(["214 outside P1, 6m", "alr_1", "/#/map?alert=alr_1"]);

    // Tapped with the app open: that window goes there. Closed: it opens there.
    const click = async () => {
      let closed = false;
      let p: Promise<unknown> = Promise.resolve();
      listeners.notificationclick({ notification: { data: { url: "/#/map?alert=alr_1" }, close: () => (closed = true) }, waitUntil: (x: Promise<unknown>) => (p = x) });
      await p;
      return closed;
    };
    expect(await click()).toBe(true);
    expect(opened).toEqual([`${ORIGIN}/#/map?alert=alr_1`]);
    windows.push({ url: `${ORIGIN}/#/data`, focused: false, messages: [] });
    await click();
    expect(windows[0].focused).toBe(true);
    expect(windows[0].messages).toEqual([{ type: "open", url: `${ORIGIN}/#/map?alert=alr_1` }]);
  });
});
