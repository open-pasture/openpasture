// Dragging a lasso on the map: the line follows the pointer, and on release the path closes
// into a ring. Shared by the Lasso tool and shift-drag (overlay.ts). Type-only MapLibre
// imports: the map is handed in, so this stays out of the first bundle.

import type { GeoJSONSource, Map as MLMap, MapMouseEvent, MapTouchEvent } from "maplibre-gl";
import type { LonLat, PositionItem } from "../../api";
import { caught, closeRing, thin } from "./lasso";

const SRC = "c-lasso";
const FG = "#F3F2EA"; // C.fg (map/base.ts loads MapLibre, so it isn't imported here)

// The lasso's line and faint fill, drawn once at the given height.
export function lassoLayers(map: MLMap, beforeId: string) {
  if (map.getSource(SRC)) return;
  map.addSource(SRC, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  const before = map.getLayer(beforeId) ? beforeId : undefined;
  map.addLayer({ id: "c-lasso-fill", type: "fill", source: SRC, filter: ["==", ["geometry-type"], "Polygon"], paint: { "fill-color": FG, "fill-opacity": 0.05 } }, before);
  map.addLayer({ id: "c-lasso-line", type: "line", source: SRC, paint: { "line-color": FG, "line-width": 1, "line-opacity": 0.8, "line-dasharray": [2, 2] } }, before);
}

// Show a closed ring, an open path while dragging, or nothing.
export function setLasso(map: MLMap, shape?: { ring?: LonLat[]; path?: LonLat[] }) {
  const src = map.getSource(SRC) as GeoJSONSource | undefined;
  if (!src) return;
  const f: GeoJSON.Feature[] = [];
  if (shape?.ring) f.push({ type: "Feature", properties: {}, geometry: { type: "Polygon", coordinates: [shape.ring] } });
  else if (shape?.path && shape.path.length > 1) f.push({ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: shape.path } });
  src.setData({ type: "FeatureCollection", features: f });
}

export function removeLasso(map: MLMap) {
  try {
    for (const id of ["c-lasso-line", "c-lasso-fill"]) if (map.getLayer(id)) map.removeLayer(id);
    if (map.getSource(SRC)) map.removeSource(SRC);
  } catch {
    /* the map went first */
  }
}

// Follow one press from `start` until the button comes up, drawing the path, then hand
// back the closed ring (undefined when it encloses nothing). The map doesn't pan meanwhile.
export function drag(map: MLMap, start: MapMouseEvent, done: (ring: LonLat[] | undefined) => void): () => void {
  start.preventDefault();
  const path: LonLat[] = [[start.lngLat.lng, start.lngLat.lat]];
  const panned = map.dragPan.isEnabled();
  map.dragPan.disable();
  let raf = 0;
  const move = (e: MapMouseEvent) => {
    path.push([e.lngLat.lng, e.lngLat.lat]);
    if (!raf) raf = requestAnimationFrame(() => {
      raf = 0;
      setLasso(map, { path });
    });
  };
  const stop = () => {
    cancelAnimationFrame(raf);
    map.off("mousemove", move);
    window.removeEventListener("mouseup", up);
    if (panned) map.dragPan.enable();
  };
  const up = () => {
    stop();
    const ring = closeRing(thin(path, 0.5));
    setLasso(map, ring ? { ring } : undefined);
    done(ring);
  };
  map.on("mousemove", move);
  window.addEventListener("mouseup", up);
  return stop;
}

// The same by touch (M): one finger drags the lasso, the map doesn't pan meanwhile, lifting it
// closes the ring. A second finger gives the gesture back to the map (pinch to zoom).
export function dragTouch(map: MLMap, start: MapTouchEvent, done: (ring: LonLat[] | undefined) => void): () => void {
  start.preventDefault();
  const path: LonLat[] = [[start.lngLat.lng, start.lngLat.lat]];
  const panned = map.dragPan.isEnabled();
  map.dragPan.disable();
  let raf = 0;
  const move = (e: MapTouchEvent) => {
    if (e.originalEvent.touches.length !== 1) return stop();
    path.push([e.lngLat.lng, e.lngLat.lat]);
    if (!raf) raf = requestAnimationFrame(() => {
      raf = 0;
      setLasso(map, { path });
    });
  };
  const stop = () => {
    cancelAnimationFrame(raf);
    map.off("touchmove", move);
    map.off("touchend", end);
    map.off("touchcancel", stop);
    if (panned) map.dragPan.enable();
  };
  const end = () => {
    stop();
    const ring = closeRing(thin(path, 0.5));
    setLasso(map, ring ? { ring } : undefined);
    done(ring);
  };
  map.on("touchmove", move);
  map.on("touchend", end);
  map.on("touchcancel", stop);
  return stop;
}

// The collars whose last position lies inside the ring.
export function caughtIn(ring: LonLat[], positions: ReadonlyMap<string, PositionItem>): string[] {
  return caught(ring, [...positions].map(([id, p]) => [id, p.fix.point] as const));
}

// Tools start from their key; there is no other way into the tools registry from outside
// the map view, so press it.
export function startTool(key: string) {
  window.dispatchEvent(new KeyboardEvent("keydown", { key }));
}

// No tool open: the tools bar holds only its buttons and the Draw menu.
export function toolsIdle(map: MLMap): boolean {
  const bar = map.getContainer().parentElement?.querySelector(".tools");
  return !bar || [...bar.children].every((el) => el.matches("button.btn, .menuwrap"));
}
