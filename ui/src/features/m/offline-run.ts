// Keeping and restoring the offline copy (offline.ts has the pure parts). While the server answers
// the copy is written every 30 s (positions move without telling the store's listeners), as soon as
// the herd's collars have loaded, and when the page is hidden or closed.

import { getToken } from "../../api";
import { store, setLastSeen } from "../../store";
import { me } from "../../store/me";
import { decode, encode, KEY, restore, signedInAs } from "./offline";

const EVERY_MS = 30_000;

export function startOffline() {
  let kept = -1;
  const save = () => {
    const s = store.get();
    // A restored copy isn't written back until the server has answered again.
    if (!s.up) return;
    const snap = encode(s, me.get(), new Date(), signedInAs(getToken()));
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
    // Refused (signed out elsewhere, the token revoked): the copy isn't this browser's to show.
    if (s.needToken) forgetLastSeen();
    else if (s.up && s.state && s.collars.length !== kept) save();
  });
  setInterval(save, EVERY_MS);
  addEventListener("pagehide", save);
  document.addEventListener("visibilitychange", () => document.visibilityState === "hidden" && save());
  setLastSeen(() => {
    const snap = decode(localStorage.getItem(KEY), signedInAs(getToken()));
    if (!snap) return undefined;
    me.set(snap.me);
    return restore(snap);
  });
}

// Signing out: the next person on this browser doesn't get the farm as the last one saw it.
export function forgetLastSeen() {
  try {
    localStorage.removeItem(KEY);
  } catch {
    /* storage off */
  }
}
