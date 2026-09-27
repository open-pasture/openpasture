// Layers > Coverage (stream G): 10 m squares under the paddock outlines (slot-fill), faint where
// fixes are good and strong where they are weak, with accuracy · fixes to switch between the two
// maps. Hovering a square shows its value.

import { createElement } from "react";
import { createRoot } from "react-dom/client";
import type { GeoJSONSource, MapMouseEvent } from "maplibre-gl";
import { gApi, type CoverageMetric } from "../../api/g";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { Segmented } from "../../ui";
import { unitsNow } from "../../units";
import { cellText, squares } from "./model";

// map/base.ts C, inlined so this chunk doesn't load MapLibre's module for three colours.
const C = { grass: "#9FD760", warn: "#F0936C", red: "#E5484D" } as const;
const SRC = "g-coverage";
const FILL = "g-coverage";
const METRIC_KEY = "openpasture.coverage.metric";
const EVERY = 10 * 60_000;

const METRICS: { value: CoverageMetric; label: string }[] = [
  { value: "accuracy", label: "accuracy" },
  { value: "fixes", label: "fixes" },
];

export function mountCoverage(ctx: OverlayCtx): OverlayHandle {
  const { map } = ctx;
  let metric: CoverageMetric = localStorage.getItem(METRIC_KEY) === "fixes" ? "fixes" : "accuracy";
  let gone = false;

  map.addSource(SRC, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  map.addLayer({
    id: FILL, type: "fill", source: SRC,
    paint: {
      "fill-color": ["match", ["get", "tone"], "good", C.grass, "fair", C.warn, C.red] as unknown as string,
      "fill-opacity": ["match", ["get", "tone"], "good", 0.18, 0.5] as unknown as number,
      "fill-antialias": false,
    },
  }, ctx.beforeId("slot-fill"));

  const load = async () => {
    try {
      const c = await gApi.coverage({ metric, cell_m: 10 });
      if (gone || c.metric !== metric) return;
      (map.getSource(SRC) as GeoJSONSource | undefined)?.setData(squares(c) as GeoJSON.FeatureCollection);
    } catch {
      // Keep the squares already drawn.
    }
  };

  // accuracy · fixes, just above the Layers button.
  const box = document.createElement("div");
  box.className = "gcov";
  map.getContainer().appendChild(box);
  const root = createRoot(box);
  const render = () =>
    root.render(createElement(Segmented<CoverageMetric>, {
      label: "Coverage", value: metric, options: METRICS,
      onChange: (m: CoverageMetric) => {
        metric = m;
        localStorage.setItem(METRIC_KEY, m);
        render();
        void load();
      },
    }));

  const tip = document.createElement("div");
  tip.className = "gcovtip mono";
  map.getContainer().appendChild(tip);
  const hover = (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
    const v = Number(e.features?.[0]?.properties?.v);
    if (!Number.isFinite(v)) return void (tip.style.display = "none");
    tip.textContent = cellText(metric, v, unitsNow());
    tip.style.display = "block";
    tip.style.transform = `translate(${e.point.x + 12}px, ${e.point.y - 24}px)`;
  };
  const leave = () => void (tip.style.display = "none");
  map.on("mousemove", FILL, hover);
  map.on("mouseleave", FILL, leave);

  render();
  void load();
  const timer = setInterval(() => void load(), EVERY);

  return {
    destroy() {
      gone = true;
      clearInterval(timer);
      map.off("mousemove", FILL, hover);
      map.off("mouseleave", FILL, leave);
      // Not while React may be committing the Layers menu.
      setTimeout(() => root.unmount(), 0);
      box.remove();
      tip.remove();
      if (map.getLayer(FILL)) map.removeLayer(FILL);
      if (map.getSource(SRC)) map.removeSource(SRC);
    },
  };
}
