// A notification opens the app at "#/map?alert=<id>": the map, flown to that alert and its herd once
// the alert list has it. The service worker hands the URL to an open window the same way.

import { alerts } from "../../store/a-engine";
import { focus } from "../a-engine/focus";
import { alertOfHash } from "./links";

function openAlert(id: string) {
  history.replaceState(null, "", "#/map");
  const found = () => alerts.get().list.find((a) => a.id === id);
  const now = found();
  if (now) return focus(now);
  // Not loaded yet: wait for the list, a little while.
  const off = alerts.subscribe(() => {
    const a = found();
    if (!a) return;
    off();
    clearTimeout(t);
    focus(a);
  });
  const t = setTimeout(off, 15_000);
}

export function startOpen() {
  const check = () => {
    const id = alertOfHash(location.hash);
    if (id) openAlert(id);
  };
  addEventListener("hashchange", check);
  check();
  if ("serviceWorker" in navigator)
    navigator.serviceWorker.addEventListener("message", (e: MessageEvent) => {
      const d = e.data as { type?: string; url?: string } | null;
      if (d?.type !== "open" || !d.url) return;
      const u = new URL(d.url, location.origin);
      if (u.origin === location.origin) location.hash = u.hash.replace(/^#/, "");
    });
}
