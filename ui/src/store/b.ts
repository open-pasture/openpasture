// Stream B's state: the paddock layer values, grazing signals per herd, a decision drawn faint
// on the map, and what the map should show next (an animal, a paddock, a sheet).

import type { ReactNode } from "react";
import type { Polygon } from "../api";
import { b, type HerdSignals, type Layers } from "../api/b";
import { store } from "../store";
import { parseHash } from "../util";
import { createSlice } from "./slice";

// ---- layer values ---------------------------------------------------------------------

export const layerData = createSlice<Layers | null>(null);

let loadingLayers: Promise<void> | undefined;
export function loadLayers() {
  loadingLayers ??= b
    .layers()
    .then((l) => layerData.set(l))
    .catch(() => {
      /* offline or no token yet: the next reconnect loads them */
    })
    .finally(() => {
      loadingLayers = undefined;
    });
  return loadingLayers;
}

// ---- grazing signals, per herd ------------------------------------------------------------

export const herdSignals = createSlice<Record<string, HerdSignals>>({});
const signalsAt = new Map<string, number>();
const SIGNALS_FRESH_MS = 60_000;

// The herd's signals, fetched at most once a minute unless `force`.
export async function loadSignals(herdId: string, force = false) {
  const at = signalsAt.get(herdId);
  if (!force && at !== undefined && Date.now() - at < SIGNALS_FRESH_MS) return;
  signalsAt.set(herdId, Date.now());
  try {
    const s = await b.signals(herdId);
    herdSignals.set((m) => ({ ...m, [herdId]: s }));
  } catch {
    signalsAt.delete(herdId);
  }
}

// ---- the map's next focus -------------------------------------------------------------------

// A decision's shape, drawn faint on the map until Esc (Data > Decisions).
export interface Ghost { id: string; geometry: Polygon }
export const ghost = createSlice<Ghost | null>(null);

export type BBox = [number, number, number, number];
export type Focus = { kind: "collar"; id: string } | { kind: "bbox"; bbox: BBox } | { kind: "sheet"; node: ReactNode };
export const focus = createSlice<(Focus & { seq: number }) | null>(null);
let seq = 0;

// West, south, east, north of a shape's outer ring.
export function bboxOf(g: Polygon): BBox | undefined {
  const r = g.coordinates[0] ?? [];
  if (!r.length) return undefined;
  const xs = r.map((p) => p[0]), ys = r.map((p) => p[1]);
  return [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)];
}

// Go to the map (if elsewhere) and show this there once it is up.
export function show(f: Focus) {
  focus.set({ ...f, seq: ++seq });
  if (parseHash(location.hash)[0] !== "map") location.hash = "/map";
}

// Draw a decision's shape on the map.
export function showGhost(g: Ghost) {
  ghost.set(g);
  if (parseHash(location.hash)[0] !== "map") location.hash = "/map";
}

// "/" opens the search box in the top bar.
export const searchOpen = createSlice(false);

// ---- keeping the layer values current ---------------------------------------------------------

let started = false;
const REFRESH_MS = 10 * 60_000;

export function startB() {
  if (started) return;
  started = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  // Rest days move with moves and decisions; reload a moment after they settle.
  const soon = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      void loadLayers();
      signalsAt.clear();
      const h = store.get().herdId;
      if (h) void loadSignals(h, true);
    }, 2000);
  };
  store.on("decision", (e) => e.decision.status === "applied" && soon());
  store.on("move", (e) => e.move.status !== "sweeping" && soon());
  store.on("resync", soon);
  let up = false, ready = false;
  let paddocks = store.get().state?.paddocks;
  const check = () => {
    const s = store.get();
    const have = s.ready && !!s.state?.farm;
    // First load once the farm is known, again when the socket comes back or paddocks change.
    if ((have && !ready) || (s.up && !up && have)) void loadLayers();
    else if (have && s.state!.paddocks !== paddocks) soon();
    ready = have;
    up = s.up;
    paddocks = s.state?.paddocks;
  };
  store.subscribe(check);
  check();
  setInterval(() => {
    if (store.get().state?.farm && document.visibilityState !== "hidden") void loadLayers();
  }, REFRESH_MS);
}
