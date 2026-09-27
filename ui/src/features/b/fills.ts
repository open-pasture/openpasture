// The Rest, NDVI, Drought and Flood layers: a stepped fill per paddock in slot-fill (above the
// imagery, under the paddock outlines) and the value under each paddock's name. Loads with the map.

import type { Marker } from "maplibre-gl";
import type { PaddockLayer } from "../../api/b";
import { centroid } from "../../geo";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { store } from "../../store";
import { layerData } from "../../store/b";
import {
  droughtFill, droughtLabel, floodLabel, floodOpacity, ndviFill, ndviLabel, restFill, restLabel, TONE, type Fill, type LayerKind,
} from "./ramp";

const FILL: Record<Exclude<LayerKind, "flood">, (r: PaddockLayer) => Fill | undefined> = {
  rest: (r) => restFill(r.rest_days, r.grazing),
  ndvi: (r) => ndviFill(r.ndvi),
  drought: (r) => droughtFill(r.drought),
};
const LABEL: Record<LayerKind, (r: PaddockLayer) => string | undefined> = { rest: restLabel, ndvi: ndviLabel, drought: droughtLabel, flood: floodLabel };

// Floodplain hatch: short horizontal dashes, staggered, like water lines. 8 css px square.
const PR = 2;
function waterBitmap(hex: string) {
  const n = 8 * PR;
  const data = new Uint8Array(n * n * 4);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  const put = (x: number, y: number) => {
    for (let dy = 0; dy < PR; dy++) for (let dx = 0; dx < PR; dx++) data.set([r, g, b, 255], ((y * PR + dy) * n + x * PR + dx) * 4);
  };
  for (let x = 0; x < 3; x++) put(x, 1);
  for (let x = 4; x < 7; x++) put(x, 5);
  return { width: n, height: n, data };
}

const empty = (): GeoJSON.FeatureCollection => ({ type: "FeatureCollection", features: [] });

// Layers on at once stack their values under the name, in the order they were turned on.
const on: LayerKind[] = [];
const redraws = new Set<() => void>();
const restack = () =>
  redraws.forEach((f) => {
    try {
      f();
    } catch {
      /* the map went first (leaving the view) */
    }
  });

export function mountFill(ctx: OverlayCtx, kind: LayerKind): OverlayHandle {
  const { map } = ctx;
  const src = `b-${kind}`;
  const layer = `${src}-fill`;
  map.addSource(src, { type: "geojson", data: empty() });
  if (kind === "flood") {
    if (!map.hasImage("b-water")) map.addImage("b-water", waterBitmap(TONE.fg), { pixelRatio: PR });
    map.addLayer({ id: layer, type: "fill", source: src, paint: { "fill-pattern": "b-water", "fill-opacity": ["get", "o"] } }, ctx.beforeId("slot-fill"));
  } else {
    map.addLayer({ id: layer, type: "fill", source: src, paint: { "fill-color": ["get", "c"], "fill-opacity": ["get", "o"] } }, ctx.beforeId("slot-fill"));
  }

  // Values under the paddock names: DOM markers, like the names (no glyph server).
  const markers = new Map<string, Marker>();
  let MarkerClass: typeof Marker | undefined;
  let gone = false;
  void import("maplibre-gl").then((m) => {
    if (gone) return;
    MarkerClass = m.Marker;
    draw();
  });

  function draw() {
    const offset: [number, number] = [0, 9 + 14 * Math.max(0, on.indexOf(kind))];
    const rows = layerData.get()?.paddocks ?? [];
    const paddocks = store.get().state?.paddocks ?? [];
    const byId = new Map(rows.map((r) => [r.paddock_id, r]));
    const features: GeoJSON.Feature[] = [];
    const labels = new Map<string, { at: [number, number]; text: string }>();
    for (const p of paddocks) {
      const r = byId.get(p.id);
      if (!r) continue;
      if (kind === "flood") {
        const o = floodOpacity(r.flood);
        if (o !== undefined) features.push({ type: "Feature", properties: { o }, geometry: p.geometry });
      } else {
        const f = FILL[kind](r);
        if (f) features.push({ type: "Feature", properties: { c: f.color, o: f.opacity }, geometry: p.geometry });
      }
      const text = LABEL[kind](r);
      if (text) labels.set(p.id, { at: centroid(p.geometry), text });
    }
    (map.getSource(src) as { setData(d: GeoJSON.FeatureCollection): void } | undefined)?.setData({ type: "FeatureCollection", features });
    if (!MarkerClass) return;
    for (const [id, m] of markers)
      if (!labels.has(id)) {
        m.remove();
        markers.delete(id);
      }
    for (const [id, l] of labels) {
      let m = markers.get(id);
      if (!m) {
        const el = document.createElement("div");
        el.className = "blabel";
        el.dataset.layer = kind;
        m = new MarkerClass({ element: el, anchor: "top", offset }).setLngLat(l.at).addTo(map);
        markers.set(id, m);
      }
      m.setLngLat(l.at).setOffset(offset);
      m.getElement().textContent = l.text;
    }
  }

  on.push(kind);
  redraws.add(draw);
  restack();
  const offLayers = layerData.subscribe(draw);
  let paddocks = store.get().state?.paddocks;
  const offStore = store.subscribe(() => {
    const next = store.get().state?.paddocks;
    if (next !== paddocks) {
      paddocks = next;
      draw();
    }
  });

  return {
    update: draw,
    destroy() {
      gone = true;
      redraws.delete(draw);
      if (on.includes(kind)) on.splice(on.indexOf(kind), 1);
      offLayers();
      offStore();
      for (const m of markers.values()) m.remove();
      markers.clear();
      try {
        if (map.getLayer(layer)) map.removeLayer(layer);
        if (map.getSource(src)) map.removeSource(src);
      } catch {
        /* the map went first (leaving the view) */
      }
      restack();
    },
  };
}
