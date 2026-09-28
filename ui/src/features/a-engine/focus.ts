// Clicking an alert anywhere flies the map to it and rings its animals. The map overlay
// hands its context over while the map is up; from another view the click opens the map
// first and the overlay finishes the job when it mounts.

import type { ReactNode } from "react";
import type { Alert, LonLat } from "../../api";
import type { OverlayCtx } from "../../map/overlays";
import { store } from "../../store";
import { collarsOf } from "./model";
import { pickAnimal, showMap } from "../m/select";

let map: OverlayCtx | null = null;
let pending: Alert | null = null;
let clear: ReturnType<typeof setTimeout> | undefined;

export function attach(ctx: OverlayCtx) {
  map = ctx;
  if (pending) {
    const a = pending;
    pending = null;
    setTimeout(() => focus(a), 250);
  }
}

export function detach() {
  map = null;
  clearTimeout(clear);
}

// Where to look: the animals' latest positions, else the alert's own place.
function points(ctx: OverlayCtx, a: Alert): LonLat[] {
  const pos = ctx.positions();
  const pts = collarsOf(a).flatMap((id) => {
    const p = pos.get(id)?.fix.point;
    return p ? [p] : [];
  });
  return pts.length ? pts : a.at ? [a.at] : [];
}

export function focus(a: Alert) {
  if (a.herd_id && store.get().herdId !== a.herd_id) store.setHerd(a.herd_id);
  if (!map) {
    pending = a;
    location.hash = "/map";
    return;
  }
  const pts = points(map, a);
  if (pts.length === 1) map.flyTo(pts[0]);
  else if (pts.length > 1) {
    const lon = pts.map((p) => p[0]), lat = pts.map((p) => p[1]);
    map.flyTo([Math.min(...lon), Math.min(...lat), Math.max(...lon), Math.max(...lat)]);
  }
  const ctx = map;
  ctx.highlight(collarsOf(a), a.severity === "critical" ? "red" : "warn");
  // On a phone the map comes out from under the sheet; one animal is picked, to walk to (M).
  const ids = collarsOf(a);
  if (ids.length === 1) pickAnimal(ids[0]);
  else showMap();
  clearTimeout(clear);
  clear = setTimeout(() => ctx.highlight([]), 8000);
}

// The map's side sheet, when the map is up.
export function openSheet(node: ReactNode | null): boolean {
  if (!map) return false;
  map.openSheet(node);
  return true;
}
