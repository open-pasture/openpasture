// Layers > Cues (stream H): a pixel tick wherever the selected herd's collars played a cue in
// the last 7 days, 2 m cells, warn tone where only warnings played and red where an animal
// crossed. The counts show under the pointer. Loaded only when the layer is turned on.

import type { GeoJSONSource, MapMouseEvent } from "maplibre-gl";
import { hApi } from "../../api/h";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { store } from "../../store";
import { tickFeatures, tickText } from "./model";

// map/base.ts C, inlined so this chunk doesn't load MapLibre's module for two colours.
const C = { warn: "#F0936C", red: "#E5484D" } as const;
const SRC = "h-cues";
const LAYER = "h-cues";
const EVERY = 60_000;
const AFTER_CUES = 5_000;
const PR = 2;

// A 3 px square, the map's pixel style.
function tick(hex: string) {
  const n = 3 * PR;
  const data = new Uint8Array(n * n * 4);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  for (let i = 0; i < n * n; i++) data.set([r, g, b, 255], i * 4);
  return { width: n, height: n, data };
}

export function mountCues(ctx: OverlayCtx): OverlayHandle {
  const { map } = ctx;
  let gone = false;
  let herd = ctx.herdId();
  for (const [id, hex] of [["h-tick-warn", C.warn], ["h-tick-out", C.red]] as const) {
    if (!map.hasImage(id)) map.addImage(id, tick(hex), { pixelRatio: PR });
  }
  map.addSource(SRC, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  map.addLayer({
    id: LAYER, type: "symbol", source: SRC,
    layout: {
      "icon-image": ["match", ["get", "kind"], "outside", "h-tick-out", "h-tick-warn"],
      "icon-size": ["interpolate", ["linear"], ["zoom"], 13, 0.6, 16, 1, 19, 1.6],
      "icon-allow-overlap": true, "icon-ignore-placement": true,
    },
    paint: { "icon-opacity": ["get", "weight"] },
  }, ctx.beforeId("slot-points"));

  const load = async () => {
    const h = herd;
    if (!h) return;
    try {
      const p = await hApi.points({ herd_id: h, from: "-7d" });
      if (gone || h !== herd) return;
      (map.getSource(SRC) as GeoJSONSource | undefined)?.setData(tickFeatures(p.ticks) as GeoJSON.FeatureCollection);
    } catch {
      // Keep the ticks already drawn.
    }
  };

  const tip = document.createElement("div");
  tip.className = "htip mono";
  map.getContainer().appendChild(tip);
  const hover = (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
    const p = e.features?.[0]?.properties;
    if (!p) return void (tip.style.display = "none");
    tip.textContent = tickText(Number(p.w), Number(p.o));
    tip.style.display = "block";
    tip.style.transform = `translate(${e.point.x + 12}px, ${e.point.y - 24}px)`;
  };
  const leave = () => void (tip.style.display = "none");
  map.on("mousemove", LAYER, hover);
  map.on("mouseleave", LAYER, leave);
  // No hover on a touch screen: a tap shows the counts, moving the map hides them (M).
  map.on("click", LAYER, hover);
  map.on("movestart", leave);

  // New cues of this herd show a few seconds after they arrive.
  let soon: ReturnType<typeof setTimeout> | undefined;
  const off = store.on("cue_batch", (e) => {
    if (e.herd_id !== herd || soon) return;
    soon = setTimeout(() => {
      soon = undefined;
      void load();
    }, AFTER_CUES);
  });
  void load();
  const timer = setInterval(() => void load(), EVERY);

  return {
    update() {
      if (ctx.herdId() === herd) return;
      herd = ctx.herdId();
      (map.getSource(SRC) as GeoJSONSource | undefined)?.setData({ type: "FeatureCollection", features: [] });
      void load();
    },
    destroy() {
      gone = true;
      off();
      clearInterval(timer);
      if (soon) clearTimeout(soon);
      map.off("mousemove", LAYER, hover);
      map.off("mouseleave", LAYER, leave);
      map.off("click", LAYER, hover);
      map.off("movestart", leave);
      tip.remove();
      if (map.getLayer(LAYER)) map.removeLayer(LAYER);
      if (map.getSource(SRC)) map.removeSource(SRC);
    },
  };
}
