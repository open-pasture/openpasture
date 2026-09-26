import * as maplibregl from "maplibre-gl";
import type { Map as MLMap, StyleSpecification } from "maplibre-gl";
import "maplibre-gl/dist/maplibre-gl.css";
// Bundle the worker ourselves; its default URL is relative to the library file, which bundling moves.
import workerUrl from "maplibre-gl/dist/maplibre-gl-worker.mjs?worker&url";
import type { LonLat, Polygon } from "../api";
import { bbox } from "../geo";

maplibregl.setWorkerUrl(workerUrl);

export const C = {
  bg: "#0B0C09",
  fg: "#F3F2EA",
  fg2: "#B7B8A6",
  fg3: "#838674",
  line2: "#333A2A",
  grass: "#9FD760",
  grass2: "#B3E27C",
  blaze: "#FF6A2B",
  warn: "#F0936C",
  red: "#E5484D",
  ink: "#0C1606",
} as const;

const style = (dim: number): StyleSpecification => ({
  version: 8,
  sources: {
    sat: {
      type: "raster",
      tiles: ["https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}"],
      tileSize: 256,
      maxzoom: 19,
      attribution: "Imagery © Esri, Maxar, Earthstar Geographics",
    },
  },
  layers: [
    { id: "bg", type: "background", paint: { "background-color": C.bg } },
    {
      id: "sat",
      type: "raster",
      source: "sat",
      // Dimmed and a little desaturated so grass and blaze overlays read.
      paint: { "raster-brightness-max": dim, "raster-saturation": -0.25, "raster-contrast": 0.04, "raster-fade-duration": 200 },
    },
  ],
});

export function createMap(container: HTMLElement, o: { center?: LonLat; zoom?: number; dim?: number; interactive?: boolean } = {}): MLMap {
  const map = new maplibregl.Map({
    container,
    style: style(o.dim ?? 0.72),
    center: o.center ?? [-98, 39],
    zoom: o.zoom ?? (o.center ? 16 : 3),
    attributionControl: { compact: false },
    interactive: o.interactive ?? true,
    dragRotate: false,
    pitchWithRotate: false,
    fadeDuration: 150,
  });
  map.touchZoomRotate.disableRotation();
  map.once("load", () => ready.add(map));
  return map;
}

export function fitPolys(map: MLMap, polys: Polygon[], padding: number | maplibregl.PaddingOptions = 80, animate = false) {
  const b = bbox(polys);
  if (b) map.fitBounds([[b[0], b[1]], [b[2], b[3]]], { padding, animate, maxZoom: 18 });
}

export const fc = (features: GeoJSON.Feature[]): GeoJSON.FeatureCollection => ({ type: "FeatureCollection", features });

export function setData(map: MLMap, id: string, data: GeoJSON.FeatureCollection) {
  const src = map.getSource(id) as maplibregl.GeoJSONSource | undefined;
  if (src) src.setData(data);
  else map.addSource(id, { type: "geojson", data });
}

const ready = new WeakSet<MLMap>();
export function onLoad(map: MLMap, f: () => void) {
  if (ready.has(map)) f();
  else map.once("load", f);
}
