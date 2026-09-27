// One small store: the farm record, collars, boundary status and decisions,
// kept fresh by /api/live.

import { useSyncExternalStore } from "react";
import { api, live, onUnauthorized, type Animal, type AppState, type BoundaryStatus, type Collar, type Decision, type Escape, type LiveEvent, type Move } from "./api";

export interface Store {
  ready: boolean;
  error?: string;
  needToken: boolean;
  up: boolean;
  state: AppState | null;
  herdId?: string;
  collars: Collar[];
  animals: Animal[];
  boundary: Record<string, BoundaryStatus>;
  decisions: Decision[];
  logs: Record<string, string[]>;
}

let s: Store = { ready: false, needToken: false, up: false, state: null, collars: [], animals: [], boundary: {}, decisions: [], logs: {} };
const subs = new Set<() => void>();
const fixSubs = new Set<(e: Extract<LiveEvent, { type: "fix" }>) => void>();

function set(patch: Partial<Store>) {
  s = { ...s, ...patch };
  subs.forEach((f) => f());
}

export const store = {
  get: () => s,
  subscribe(f: () => void) {
    subs.add(f);
    return () => void subs.delete(f);
  },
  // Fixes arrive often; the map listens directly instead of re-rendering React.
  onFix(f: (e: Extract<LiveEvent, { type: "fix" }>) => void) {
    fixSubs.add(f);
    return () => void fixSubs.delete(f);
  },
  setHerd(herdId: string) {
    set({ herdId });
    void refreshHerd();
  },
  refresh,
  refreshHerd,
  tokenSaved() {
    set({ needToken: false });
    connectLive(); // at once, not after the socket's backoff
    void refresh();
  },
};

// What a collar is called in the app: its animal's tag, else its own name.
export function collarLabel(c: Collar, animals: Animal[] = s.animals) {
  return animals.find((a) => a.id === c.animal_id || a.collar_id === c.id)?.tag ?? c.name;
}

// Animals a move left behind: its stragglers while it sweeps, and after it is done those
// still not inside.
export function behindOf(move: Move | undefined, collars: Collar[] = s.collars): string[] {
  if (!move || move.status === "stopped") return [];
  return move.stragglers.filter((id) => {
    const c = collars.find((x) => x.id === id);
    return c && (move.status === "sweeping" || c.state !== "inside");
  });
}

// Escapes still bringing an animal back.
export function outOf(b: BoundaryStatus | undefined): Escape[] {
  return (b?.escapes ?? []).filter((e) => e.status === "returning");
}

export function useStore<T>(sel: (s: Store) => T): T {
  return useSyncExternalStore(store.subscribe, () => sel(s));
}

async function refresh() {
  try {
    const state = await api.state();
    const herdId = s.herdId && state.herds.some((h) => h.id === s.herdId) ? s.herdId : state.herds[0]?.id;
    set({ state, herdId, ready: true, error: undefined });
    await refreshHerd();
  } catch (e) {
    set({ ready: true, error: (e as Error).message });
  }
}

async function refreshHerd() {
  const herd = s.herdId;
  // Each part stands alone, so one missing route doesn't blank the rest.
  const [collars, animals, decisions, boundary] = await Promise.all([
    api.collars().catch(() => s.collars),
    api.animals().catch(() => s.animals),
    herd ? api.decisions(herd, 20).catch(() => s.decisions) : Promise.resolve([] as Decision[]),
    herd ? api.boundary(herd).catch(() => undefined) : Promise.resolve(undefined),
  ]);
  set({ collars, animals, decisions, boundary: herd && boundary ? { ...s.boundary, [herd]: boundary } : s.boundary });
}

let herdTimer: ReturnType<typeof setTimeout> | undefined;
function refreshSoon() {
  clearTimeout(herdTimer);
  herdTimer = setTimeout(() => void refreshHerd(), 800);
}

let boundaryTimer: ReturnType<typeof setTimeout> | undefined;
function refreshBoundarySoon(herd: string) {
  clearTimeout(boundaryTimer);
  boundaryTimer = setTimeout(async () => {
    try {
      const b = await api.boundary(herd);
      set({ boundary: { ...s.boundary, [herd]: b } });
      if (s.state) set({ state: { ...s.state, ...(await api.state()) } });
    } catch {
      /* next event retries */
    }
  }, 120);
}

function onEvent(e: LiveEvent) {
  switch (e.type) {
    case "fix": {
      fixSubs.forEach((f) => f(e));
      const i = s.collars.findIndex((c) => c.id === e.collar_id);
      if (i >= 0) {
        const collars = s.collars.slice();
        collars[i] = { ...collars[i], last_fix: e.fix, last_seen: e.fix.at, state: e.state };
        s = { ...s, collars }; // no notify: the collar list ticks on its own clock
      }
      break;
    }
    case "collar": {
      const i = s.collars.findIndex((c) => c.id === e.collar.id);
      const collars = s.collars.slice();
      if (i >= 0) collars[i] = { ...collars[i], ...e.collar };
      else {
        collars.push(e.collar);
        refreshSoon(); // its animal is usually linked right after
      }
      set({ collars });
      break;
    }
    case "ack": {
      const b = s.boundary[e.herd_id];
      if (b) {
        const acks = b.acks.filter((a) => a.collar_id !== e.collar_id);
        acks.push({ collar_id: e.collar_id, version: e.version, status: e.status, reason: e.reason, at: new Date().toISOString() });
        set({ boundary: { ...s.boundary, [e.herd_id]: { ...b, acks } } });
        // A collar on or back from its own boundary: the server says which herd version that counts as.
        if (b.escapes?.some((x) => x.collar_id === e.collar_id)) refreshBoundarySoon(e.herd_id);
      }
      break;
    }
    case "boundary":
      refreshBoundarySoon(e.herd_id);
      break;
    case "decision": {
      const d = e.decision;
      if (d.herd_id !== s.herdId) break;
      const decisions = [d, ...s.decisions.filter((x) => x.id !== d.id)].sort((a, b) => b.created_at.localeCompare(a.created_at));
      set({ decisions });
      refreshBoundarySoon(d.herd_id);
      break;
    }
    case "move": {
      // The move rides on the boundary status. Steps also send a boundary event, which refetches.
      const m = e.move;
      const b = s.boundary[m.herd_id];
      if (b) set({ boundary: { ...s.boundary, [m.herd_id]: { ...b, move: m } } });
      else refreshBoundarySoon(m.herd_id);
      break;
    }
    case "escape": {
      const x = e.escape;
      const b = s.boundary[x.herd_id];
      if (b) set({ boundary: { ...s.boundary, [x.herd_id]: { ...b, escapes: [...(b.escapes ?? []).filter((y) => y.id !== x.id), x] } } });
      else refreshBoundarySoon(x.herd_id);
      break;
    }
    case "decision_log":
      set({ logs: { ...s.logs, [e.decision_id]: [...(s.logs[e.decision_id] ?? []), e.line] } });
      break;
    case "resync":
      void refresh();
      break;
    case "cue":
      break;
  }
}

let stopLive: (() => void) | undefined;
function connectLive() {
  stopLive?.();
  stopLive = live(onEvent, (up) => {
    set({ up });
    if (up && s.ready) void refresh();
  });
}

export function start() {
  onUnauthorized(() => set({ needToken: true }));
  void refresh();
  connectLive();
  return () => {
    stopLive?.();
    stopLive = undefined;
  };
}
