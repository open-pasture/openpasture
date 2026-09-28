// The last farm this browser saw, for when the server can't be reached: kept in localStorage while
// the app runs, and put back at start when the first read fails for want of a network, so the map
// opens on the last known places (dimmed, with their age beside "offline") instead of blank. Never
// anything from /api is cached by the service worker; this is the app's own copy.

import type { Animal, AppState, BoundaryStatus, Collar, Me } from "../../api";
import type { Store } from "../../store";

export const KEY = "openpasture.last";
const VERSION = 1;

export interface Snapshot {
  v: typeof VERSION;
  at: string;
  me: Me;
  state: AppState;
  herdId?: string;
  collars: Collar[];
  animals: Animal[];
  boundary: Record<string, BoundaryStatus>;
}

// What goes in: the farm record without the app token, and only the selected herd's boundary.
export function encode(s: Store, me: Me | null, now: Date): Snapshot | undefined {
  if (!s.state || !me) return undefined;
  const state: AppState = s.state.settings?.server
    ? { ...s.state, settings: { ...s.state.settings, server: { ...s.state.settings.server, app_token: "" } } }
    : s.state;
  const boundary = s.herdId && s.boundary[s.herdId] ? { [s.herdId]: s.boundary[s.herdId] } : {};
  return { v: VERSION, at: now.toISOString(), me, state, herdId: s.herdId, collars: s.collars, animals: s.animals, boundary };
}

export function decode(raw: string | null): Snapshot | undefined {
  if (!raw) return undefined;
  try {
    const v = JSON.parse(raw) as Partial<Snapshot>;
    if (v.v !== VERSION || !v.state?.farm || !Array.isArray(v.collars) || !Array.isArray(v.animals) || !v.me?.role) return undefined;
    return { boundary: {}, ...v } as Snapshot;
  } catch {
    return undefined;
  }
}

// The store's fields a snapshot fills.
export function restore(snap: Snapshot): Partial<Store> {
  const herdId = snap.herdId && snap.state.herds.some((h) => h.id === snap.herdId) ? snap.herdId : snap.state.herds[0]?.id;
  return { state: snap.state, herdId, collars: snap.collars, animals: snap.animals, boundary: snap.boundary, decisions: [], up: false };
}

// The newest fix the map holds, as ms since the epoch.
export function newestFix(collars: readonly Collar[]): number | undefined {
  let best: number | undefined;
  for (const c of collars) {
    const t = c.last_fix ? Date.parse(c.last_fix.at) : NaN;
    if (Number.isFinite(t) && (best === undefined || t > best)) best = t;
  }
  return best;
}
