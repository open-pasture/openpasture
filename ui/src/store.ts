// One small store: the farm record, collars, boundary status and decisions,
// kept fresh by /api/live.

import { useSyncExternalStore } from "react";
import { api, live, onUnauthorized, type Animal, type AppState, type BoundaryStatus, type Collar, type Decision, type Escape, type LiveEvent, type LiveEventOf, type LiveEventType, type Move, type PositionItem } from "./api";
import "./api/p";
import { loadMe } from "./store/me";
import { applyAcks, applyPositions, indexById, labels } from "./store/live";

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
type FixEvent = LiveEventOf<"fix">;
const fixSubs = new Set<(e: FixEvent) => void>();
const positionSubs = new Set<(items: readonly PositionItem[], herdId: string) => void>();

// Row of each collar in s.collars, rebuilt when the list itself is replaced.
let indexed: Collar[] = s.collars;
let index = indexById(indexed);
function collarIndex() {
  if (indexed !== s.collars) {
    indexed = s.collars;
    index = indexById(indexed);
  }
  return index;
}
// Stream handlers per event type; they run before the core switch below.
const handlers = new Map<string, Set<(e: LiveEvent) => void>>();

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
  // Positions arrive in batches (one per herd every 500 ms); the map listens directly
  // instead of re-rendering React. Called once per batch.
  onPositions(f: (items: readonly PositionItem[], herdId: string) => void) {
    positionSubs.add(f);
    return () => void positionSubs.delete(f);
  },
  // Each collar's newest fix, one call per collar of a positions batch.
  onFix(f: (e: FixEvent) => void) {
    fixSubs.add(f);
    return () => void fixSubs.delete(f);
  },
  // Every live event of this type, before the store applies it. Returns unsubscribe.
  on<K extends LiveEventType>(type: K, f: (e: LiveEventOf<K>) => void) {
    let set = handlers.get(type);
    if (!set) handlers.set(type, (set = new Set()));
    const h = f as (e: LiveEvent) => void;
    set.add(h);
    return () => void set.delete(h);
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

// Every collar's label at once, for lists (one pass, not a search per collar).
let labelled: { collars: Collar[]; animals: Animal[]; map: Map<string, string> } | undefined;
export function collarLabels(collars: Collar[] = s.collars, animals: Animal[] = s.animals): Map<string, string> {
  if (!labelled || labelled.collars !== collars || labelled.animals !== animals) labelled = { collars, animals, map: labels(collars, animals) };
  return labelled.map;
}

// Animals a move left behind: its stragglers while it sweeps, and after it is done those
// still not inside.
export function behindOf(move: Move | undefined, collars: Collar[] = s.collars): string[] {
  if (!move || move.status === "stopped") return [];
  const byId = new Map(collars.map((c) => [c.id, c] as const));
  return move.stragglers.filter((id) => {
    const c = byId.get(id);
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
    const [state] = await Promise.all([api.state(), loadMe()]);
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

function emit(e: LiveEvent) {
  handlers.get(e.type)?.forEach((f) => {
    try {
      f(e);
    } catch (err) {
      console.error(`live ${e.type} handler failed`, err);
    }
  });
}

// A batch also reaches handlers of the single event, once per item, so code written for
// single fix, ack and cue events keeps working. Only when someone listens.
function fanOut(e: LiveEvent) {
  if (e.type === "positions" && handlers.get("fix")?.size)
    for (const it of e.items) emit({ type: "fix", collar_id: it.collar_id, animal_id: it.animal_id, herd_id: e.herd_id, fix: it.fix, state: it.state });
  if (e.type === "ack_batch" && handlers.get("ack")?.size)
    for (const a of e.items) emit({ type: "ack", collar_id: a.collar_id, herd_id: e.herd_id, version: a.version, status: a.status, reason: a.reason });
  if (e.type === "cue_batch" && handlers.get("cue")?.size)
    for (const c of e.items) emit({ type: "cue", collar_id: c.collar_id, at: c.at, level: c.level, margin_m: c.margin_m, kind: c.kind, ring: c.ring });
}

function onEvent(e: LiveEvent) {
  emit(e);
  fanOut(e);
  switch (e.type) {
    case "positions": {
      const collars = applyPositions(s.collars, collarIndex(), e.items);
      if (collars !== s.collars) {
        s = { ...s, collars }; // no notify: the collar list ticks on its own clock
        indexed = collars; // same rows, same index
      }
      positionSubs.forEach((f) => f(e.items, e.herd_id));
      if (fixSubs.size)
        for (const it of e.items) {
          const fx: FixEvent = { type: "fix", collar_id: it.collar_id, animal_id: it.animal_id, herd_id: e.herd_id, fix: it.fix, state: it.state };
          fixSubs.forEach((f) => f(fx));
        }
      break;
    }
    case "collar": {
      const i = collarIndex().get(e.collar.id) ?? -1;
      const collars = s.collars.slice();
      if (i >= 0) collars[i] = { ...collars[i], ...e.collar };
      else {
        collars.push(e.collar);
        refreshSoon(); // its animal is usually linked right after
      }
      set({ collars });
      break;
    }
    case "ack_batch": {
      const b = s.boundary[e.herd_id];
      if (b) {
        set({ boundary: { ...s.boundary, [e.herd_id]: { ...b, acks: applyAcks(b.acks, e.items, new Date().toISOString()) } } });
        // A collar on or back from its own boundary: the server says which herd version that counts as.
        if (b.escapes?.some((x) => e.items.some((a) => a.collar_id === x.collar_id))) refreshBoundarySoon(e.herd_id);
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
