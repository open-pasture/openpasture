// The pre-send drawing, in slot-plan while a tool's check is in: overlaps in red hatch (and the
// part past the farm boundary), roads and neighbour lines crossed in red, water inside ringed,
// weak GPS cells in warn, offline collars as hollow squares over the animals, and the sweep's
// back lines faint dashed with "~16 min". Loads with the map.

import * as maplibregl from "maplibre-gl";
import type { GeoJSONSource, Map as MLMap } from "maplibre-gl";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { hatchBitmap, PR } from "../d/icons";
import { drawing, minutesText } from "./model";
import { attach, detach, presend } from "./state";

// map/base.ts C, inlined: this chunk shouldn't pull the map module for five colours.
const C = { bg: "#0B0C09", fg: "#F3F2EA", fg3: "#838674", warn: "#F0936C", red: "#E5484D" } as const;

const SRC = { hatch: "f-hatch", lines: "f-lines", water: "f-water", waterAreas: "f-water-areas", weak: "f-weak", offline: "f-offline", back: "f-back" } as const;
const LAYERS = ["f-weak-fill", "f-hatch-fill", "f-hatch-line", "f-lines", "f-back", "f-water-areas", "f-water", "f-offline"];

// An 11 px square: a grey edge around the map's dark, so the animal under it reads hollow.
function hollow() {
  const n = 11 * PR;
  const data = new Uint8Array(n * n * 4);
  const rgb = (h: string) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
  const [gr, gg, gb] = rgb(C.fg3);
  const [br, bgc, bb] = rgb(C.bg);
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++) {
      const edge = x < PR * 1.5 || y < PR * 1.5 || x >= n - PR * 1.5 || y >= n - PR * 1.5;
      data.set(edge ? [gr, gg, gb, 255] : [br, bgc, bb, 255], (y * n + x) * 4);
    }
  return { width: n, height: n, data };
}

export function mountPresend(ctx: OverlayCtx): OverlayHandle {
  const map: MLMap = ctx.map;
  if (!map.hasImage("f-hatch")) map.addImage("f-hatch", hatchBitmap(C.red, 230), { pixelRatio: PR });
  if (!map.hasImage("f-hollow")) map.addImage("f-hollow", hollow(), { pixelRatio: PR });
  for (const id of Object.values(SRC)) map.addSource(id, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  const plan = ctx.beforeId("slot-plan");
  map.addLayer({ id: "f-weak-fill", type: "fill", source: SRC.weak, paint: { "fill-color": C.warn, "fill-opacity": 0.28 } }, plan);
  map.addLayer({ id: "f-hatch-fill", type: "fill", source: SRC.hatch, paint: { "fill-pattern": "f-hatch", "fill-opacity": 0.9 } }, plan);
  map.addLayer({ id: "f-hatch-line", type: "line", source: SRC.hatch, paint: { "line-color": C.red, "line-width": 1 } }, plan);
  map.addLayer({ id: "f-lines", type: "line", source: SRC.lines, paint: { "line-color": C.red, "line-width": 2.5 } }, plan);
  map.addLayer({ id: "f-back", type: "line", source: SRC.back, paint: { "line-color": C.fg, "line-width": 1, "line-opacity": 0.45, "line-dasharray": [2, 3] } }, plan);
  map.addLayer({ id: "f-water-areas", type: "line", source: SRC.waterAreas, paint: { "line-color": C.fg, "line-width": 2 } }, plan);
  map.addLayer({
    id: "f-water", type: "circle", source: SRC.water,
    paint: { "circle-radius": 11, "circle-color": "rgba(0,0,0,0)", "circle-stroke-color": C.fg, "circle-stroke-width": 1.5 },
  }, plan);
  // Over the animals: the hollow square is how an offline collar reads.
  map.addLayer({
    id: "f-offline", type: "symbol", source: SRC.offline,
    layout: { "icon-image": "f-hollow", "icon-allow-overlap": true, "icon-ignore-placement": true },
  }, ctx.beforeId("slot-top"));

  const el = document.createElement("div");
  el.className = "plabel fsweep";
  const label = new maplibregl.Marker({ element: el, anchor: "bottom", offset: [0, -6] });
  let shown = false;

  const draw = () => {
    const d = drawing(presend.get().result);
    for (const k of Object.keys(SRC) as (keyof typeof SRC)[]) (map.getSource(SRC[k]) as GeoJSONSource | undefined)?.setData(d[k]);
    const minutes = presend.get().result?.sweep?.minutes;
    if (d.label && minutes !== undefined) {
      el.textContent = minutesText(minutes);
      label.setLngLat(d.label);
      if (!shown) label.addTo(map);
      shown = true;
    } else if (shown) {
      label.remove();
      shown = false;
    }
  };
  const off = presend.subscribe(draw);
  draw();
  attach(ctx);
  return {
    destroy() {
      detach();
      off();
      label.remove();
      for (const id of LAYERS) if (map.getLayer(id)) map.removeLayer(id);
      for (const id of Object.values(SRC)) if (map.getSource(id)) map.removeSource(id);
    },
  };
}
