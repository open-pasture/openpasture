// Keeping and restoring the offline copy (offline.ts has the pure parts). While the server answers
// the copy is written every 30 s (positions move without telling the store's listeners), as soon as
// the herd's collars have loaded, and when the page is hidden or closed.

import { store, setLastSeen } from "../../store";
import { me } from "../../store/me";
import { decode, encode, KEY, restore } from "./offline";

const EVERY_MS = 30_000;

export function startOffline() {
  let kept = -1;
  const save = () => {
    const s = store.get();
    // A restored copy isn't written back until the server has answered again.
    if (!s.up) return;
    const snap = encode(s, me.get(), new Date());
    if (!snap) return;
    try {
      localStorage.setItem(KEY, JSON.stringify(snap));
      kept = snap.collars.length;
    } catch {
      /* storage full or off: the app works without it */
    }
  };
  // The first full read (collars in) is worth keeping at once.
  store.subscribe(() => {
    const s = store.get();
    if (s.up && s.state && s.collars.length !== kept) save();
  });
  setInterval(save, EVERY_MS);
  addEventListener("pagehide", save);
  document.addEventListener("visibilitychange", () => document.visibilityState === "hidden" && save());
  setLastSeen(() => {
    const snap = decode(localStorage.getItem(KEY));
    if (!snap) return undefined;
    me.set(snap.me);
    return restore(snap);
  });
}
