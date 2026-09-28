// This browser's push subscription: turning alerts on and off here, and keeping the subscription on
// the farm's current key. Needs a secure page, a service worker and the Push API (on an iPhone,
// only the app added to the Home Screen has it).

import { pushApi, type MySubscription, type PushView } from "../../api/m";

export const pushSupported = () =>
  typeof window !== "undefined" && window.isSecureContext && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;

// base64url → bytes, for applicationServerKey.
export function keyBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const s = b64url.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(s + "=".repeat((4 - (s.length % 4)) % 4));
  const out = new Uint8Array(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

const sameKey = (sub: PushSubscription, key: Uint8Array) => {
  const k = sub.options.applicationServerKey;
  if (!k) return false;
  const a = new Uint8Array(k);
  return a.length === key.length && a.every((x, i) => x === key[i]);
};

export async function registration(): Promise<ServiceWorkerRegistration | undefined> {
  if (!pushSupported()) return undefined;
  return (await navigator.serviceWorker.getRegistration("/")) ?? navigator.serviceWorker.register("/sw.js", { scope: "/" });
}

export async function current(): Promise<PushSubscription | null> {
  const reg = await registration();
  return (await reg?.pushManager.getSubscription()) ?? null;
}

// The server's record of this browser's subscription, if it has one.
export const mineOf = (v: PushView | undefined, sub: PushSubscription | null): MySubscription | undefined =>
  sub ? v?.mine.find((m) => m.endpoint === sub.endpoint) : undefined;

// Turn alerts on in this browser: ask for permission, subscribe with the farm's key (again, when the
// browser's subscription was made with an older one), hand it to the server.
export async function turnOn(v: PushView): Promise<MySubscription> {
  if (!v.vapid_public_key) throw new Error(v.reason ?? "Push isn't available here.");
  const perm = await Notification.requestPermission();
  if (perm !== "granted") throw new Error("Notifications are blocked for this site.");
  const reg = await registration();
  if (!reg) throw new Error("This browser can't take notifications.");
  await navigator.serviceWorker.ready;
  const key = keyBytes(v.vapid_public_key);
  let sub = await reg.pushManager.getSubscription();
  if (sub && !sameKey(sub, key)) {
    await sub.unsubscribe();
    sub = null;
  }
  sub ??= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
  return pushApi.subscribe(sub.toJSON());
}

export async function turnOff(v: PushView | undefined) {
  const sub = await current();
  const mine = mineOf(v, sub);
  if (mine) await pushApi.remove(mine.id);
  await sub?.unsubscribe();
}

// At start: a browser that had alerts on keeps them when the farm's key changed (new keys drop
// every subscription on the server). Nothing is asked of the person.
export async function keepCurrent() {
  if (!pushSupported() || Notification.permission !== "granted") return;
  const sub = await current();
  if (!sub) return;
  const v = await pushApi.get();
  if (!v.available || !v.vapid_public_key || sameKey(sub, keyBytes(v.vapid_public_key))) return;
  await turnOn(v);
}
