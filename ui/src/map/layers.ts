import * as maplibregl from "maplibre-gl";
import type { Map as MLMap } from "maplibre-gl";
import type { Paddock, Polygon } from "../api";
import { centroid } from "../geo";
import { C, fc, setData } from "./base";

// Paddock outlines, the active / pending / proposed boundaries. Labels are DOM
// markers so they can use JetBrains Mono without a glyph server.

export function addFarmLayers(map: MLMap) {
  for (const id of ["paddocks", "swept", "active", "target", "back", "chevrons", "pending", "proposed", "draft"]) setData(map, id, fc([]));
  map.addLayer({ id: "paddocks-fill", type: "fill", source: "paddocks", paint: { "fill-color": C.fg, "fill-opacity": ["case", ["boolean", ["feature-state", "hover"], false], 0.05, 0] } });
  map.addLayer({ id: "paddocks-line", type: "line", source: "paddocks", paint: { "line-color": C.fg, "line-opacity": 0.32, "line-width": 1 } });
  // A move: the ground behind the back line fades back into the imagery.
  map.addLayer({ id: "swept-fill", type: "fill", source: "swept", paint: { "fill-color": C.bg, "fill-opacity": 0, "fill-opacity-transition": { duration: 700, delay: 0 } } });
  map.addLayer({ id: "active-fill", type: "fill", source: "active", paint: { "fill-color": C.grass, "fill-opacity": 0.07 } });
  map.addLayer({ id: "active-line", type: "line", source: "active", paint: { "line-color": C.grass, "line-width": 1.5 } });
  // A running move's target: thin, dashed, no fill. The active line sweeps toward it.
  map.addLayer({ id: "target-line", type: "line", source: "target", paint: { "line-color": C.grass2, "line-width": 1.5, "line-dasharray": [3, 2.5] } });
  // The back line, the edge doing the pushing: brighter, with a soft glow.
  map.addLayer({ id: "back-glow", type: "line", source: "back", layout: { "line-cap": "round" }, paint: { "line-color": C.grass, "line-width": 14, "line-blur": 10, "line-opacity": 0, "line-opacity-transition": { duration: 700, delay: 0 } } });
  map.addLayer({ id: "back-line", type: "line", source: "back", layout: { "line-cap": "round" }, paint: { "line-color": C.grass2, "line-width": 3, "line-opacity": 0, "line-opacity-transition": { duration: 700, delay: 0 } } });
  // Faint chevrons drifting from the back line toward the target.
  map.addLayer({
    id: "chevrons", type: "symbol", source: "chevrons",
    layout: { "icon-image": "chevron", "icon-size": 1.25, "icon-rotate": ["get", "rot"], "icon-rotation-alignment": "map", "icon-allow-overlap": true, "icon-ignore-placement": true },
    paint: { "icon-opacity": ["get", "o"] },
  });
  map.addLayer({ id: "pending-line", type: "line", source: "pending", paint: { "line-color": C.grass, "line-width": 1.5, "line-dasharray": [2, 2] } });
  map.addLayer({ id: "proposed-fill", type: "fill", source: "proposed", paint: { "fill-color": C.blaze, "fill-opacity": 0.07 } });
  map.addLayer({ id: "proposed-line", type: "line", source: "proposed", paint: { "line-color": C.blaze, "line-width": 1.5, "line-dasharray": [4, 3] } });
}

const poly = (g: Polygon, props: Record<string, unknown> = {}, id?: number): GeoJSON.Feature => ({ type: "Feature", id, properties: props, geometry: g });

export function setPaddocks(map: MLMap, paddocks: Paddock[]) {
  setData(map, "paddocks", fc(paddocks.map((p, i) => poly(p.geometry, { id: p.id, name: p.name }, i + 1))));
}

export function setBoundary(map: MLMap, which: "active" | "target" | "swept" | "pending" | "proposed", g?: Polygon) {
  setData(map, which, fc(g ? [poly(g)] : []));
}

export class Labels {
  private markers = new Map<string, maplibregl.Marker>();
  constructor(private map: MLMap) {}
  set(items: { id: string; at: [number, number]; text: string; tone?: "grass" | "blaze" }[]) {
    const seen = new Set<string>();
    for (const it of items) {
      seen.add(it.id);
      let m = this.markers.get(it.id);
      if (!m) {
        const el = document.createElement("div");
        el.className = "plabel";
        m = new maplibregl.Marker({ element: el, anchor: "center" }).setLngLat(it.at).addTo(this.map);
        this.markers.set(it.id, m);
      }
      m.setLngLat(it.at);
      const el = m.getElement();
      el.textContent = it.text;
      el.dataset.tone = it.tone ?? "";
    }
    for (const [id, m] of this.markers)
      if (!seen.has(id)) {
        m.remove();
        this.markers.delete(id);
      }
  }
  clear() {
    this.set([]);
  }
}

export function paddockLabels(paddocks: Paddock[], grazingId?: string, proposedId?: string) {
  return paddocks.map((p) => ({
    id: p.id,
    at: centroid(p.geometry),
    text: p.name,
    tone: p.id === proposedId ? ("blaze" as const) : p.id === grazingId ? ("grass" as const) : undefined,
  }));
}

