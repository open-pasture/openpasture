import { useEffect, useState } from "react";

export function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

export function age(iso: string | undefined, now: number) {
  if (!iso) return "–";
  const s = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function clock(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(Math.floor(s / 3600))}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}`;
}

export const typing = (e: KeyboardEvent) => {
  const t = e.target as HTMLElement;
  return t.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName);
};

export function useKey(handler: (e: KeyboardEvent) => void, deps: unknown[]) {
  useEffect(() => {
    const f = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      handler(e);
    };
    window.addEventListener("keydown", f);
    return () => window.removeEventListener("keydown", f);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}

// "#/herd/214" → ["herd", "214"]; "#/" → ["map", ""]. go("herd/214") navigates.
export function parseHash(hash: string): [string, string] {
  const path = hash.replace(/^#\/?/, "");
  const i = path.indexOf("/");
  // A query right after the view is the view's: #/herd?select=… → ["herd", "?select=…"].
  const q = path.indexOf("?");
  if (q >= 0 && (i < 0 || q < i)) return [path.slice(0, q) || "map", path.slice(q)];
  const view = i < 0 ? path : path.slice(0, i);
  return [view || "map", i < 0 ? "" : path.slice(i + 1)];
}

export function useHash(): [string, string, (h: string) => void] {
  const [h, setH] = useState(() => location.hash);
  useEffect(() => {
    const f = () => setH(location.hash);
    window.addEventListener("hashchange", f);
    return () => window.removeEventListener("hashchange", f);
  }, []);
  const [view, rest] = parseHash(h);
  return [view, rest, (v) => (location.hash = "/" + v)];
}

// A form's request: busy while it runs, and when it fails its message (the server's sentence)
// goes to `failed` for the form to show, instead of an unhandled rejection and no word. True
// when it went through.
export async function attempt(f: () => Promise<unknown>, on: { busy?: (b: boolean) => void; failed: (message: string | undefined) => void }): Promise<boolean> {
  on.busy?.(true);
  on.failed(undefined);
  try {
    await f();
    return true;
  } catch (e) {
    on.failed(e instanceof Error ? e.message : String(e));
    return false;
  } finally {
    on.busy?.(false);
  }
}

// One pending timer per key (a herd): a new call replaces that key's timer only.
export function perKey<K>() {
  const timers = new Map<K, ReturnType<typeof setTimeout>>();
  return {
    set(k: K, ms: number, f: () => void) {
      clearTimeout(timers.get(k));
      timers.set(k, setTimeout(() => {
        timers.delete(k);
        f();
      }, ms));
    },
  };
}
