// Alerts on the map, in slot-top: a slow red ring on each unacked critical alert (on every
// member of a rollup), a hollow grey square at a silent collar's last fix, and an accuracy
// circle around a collar whose GPS is weak. No MapLibre import here: the overlay gets the map
// from its context once the map is up.

import type { GeoJSONSource, Map as MLMap } from "maplibre-gl";
import type { Alert, LonLat } from "../../api";
import type { Overlay, OverlayCtx } from "../../map/overlays";
import { alerts } from "../../store/a-engine";
import { attach, detach } from "./focus";
import { circle, collarsOf, memberFacts, pulse } from "./model";

// C.red, C.fg3, C.bg (map/base.ts), inlined so the registries stay out of the map bundle.
const RED = "#E5484D";
const GREY = "#838674";
const BG = "#0B0C09";
const PR = 2;

const SRC = { rings: "alert-rings", silent: "alert-silent", accuracy: "alert-accuracy" } as const;
const SILENT_KINDS = new Set(["silent", "herd_silent"]);

type FC = GeoJSON.FeatureCollection;
const fc = (features: GeoJSON.Feature[]): FC => ({ type: "FeatureCollection", features });
const point = (p: LonLat): GeoJSON.Feature => ({ type: "Feature", properties: {}, geometry: { type: "Point", coordinates: p } });

// An 11 px square: a grey edge around the map's own dark, so the animal under it reads hollow.
function hollow() {
  const n = 11 * PR;
  const data = new Uint8Array(n * n * 4);
  const rgb = (h: string) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
  const [gr, gg, gb] = rgb(GREY);
  const [br, bgc, bb] = rgb(BG);
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++) {
      const edge = x < PR * 1.5 || y < PR * 1.5 || x >= n - PR * 1.5 || y >= n - PR * 1.5;
      data.set(edge ? [gr, gg, gb, 255] : [br, bgc, bb, 255], (y * n + x) * 4);
    }
  return { width: n, height: n, data };
}

// What to draw from the unresolved alerts and the latest positions.
export function drawn(list: readonly Alert[], where: (collarId: string) => LonLat | undefined) {
  const rings: LonLat[] = [];
  const silent: LonLat[] = [];
  const accuracy: GeoJSON.Feature[] = [];
  for (const a of list) {
    if (a.status === "resolved") continue;
    const ids = collarsOf(a);
    if (a.severity === "critical" && a.status === "open") {
      const pts = ids.map(where).filter((p): p is LonLat => !!p);
      rings.push(...(pts.length ? pts : a.at ? [a.at] : []));
    }
    if (SILENT_KINDS.has(a.kind)) silent.push(...ids.map(where).filter((p): p is LonLat => !!p));
    if (a.kind === "gps_degraded") {
      for (const [id, m] of memberFacts(a)) {
        const p = where(id);
        const r = typeof m.accuracy_m === "number" ? m.accuracy_m : undefined;
        if (p && r) accuracy.push({ type: "Feature", properties: {}, geometry: { type: "Polygon", coordinates: [circle(p, r)] } });
      }
    }
  }
  return { rings, silent, accuracy };
}

export const alertOverlay: Overlay = {
  id: "a-engine",
  slot: "slot-top",
  mount(ctx: OverlayCtx) {
    const map: MLMap = ctx.map;
    const before = ctx.beforeId("slot-top");
    for (const id of Object.values(SRC)) map.addSource(id, { type: "geojson", data: fc([]) });
    if (!map.hasImage("alert-hollow")) map.addImage("alert-hollow", hollow(), { pixelRatio: PR });
    map.addLayer({ id: "alert-accuracy-fill", type: "fill", source: SRC.accuracy, paint: { "fill-color": GREY, "fill-opacity": 0.06 } }, before);
    map.addLayer({ id: "alert-accuracy-line", type: "line", source: SRC.accuracy, paint: { "line-color": GREY, "line-width": 1, "line-opacity": 0.7 } }, before);
    map.addLayer({
      id: "alert-silent", type: "symbol", source: SRC.silent,
      layout: { "icon-image": "alert-hollow", "icon-allow-overlap": true, "icon-ignore-placement": true },
    }, before);
    map.addLayer({
      id: "alert-rings", type: "circle", source: SRC.rings,
      paint: { "circle-radius": 7, "circle-color": "rgba(0,0,0,0)", "circle-stroke-color": RED, "circle-stroke-width": 1.5, "circle-stroke-opacity": 0.7 },
    }, before);

    let raf = 0;
    let last = 0;
    let ringing = false;
    const animate = (t: number) => {
      raf = 0;
      if (!ringing) return;
      if (t - last >= 50) {
        last = t;
        const { radius, opacity } = pulse(t);
        map.setPaintProperty("alert-rings", "circle-radius", radius);
        map.setPaintProperty("alert-rings", "circle-stroke-opacity", opacity);
      }
      raf = requestAnimationFrame(animate);
    };

    const set = (id: string, data: FC) => (map.getSource(id) as GeoJSONSource | undefined)?.setData(data);
    const draw = () => {
      const pos = ctx.positions();
      const d = drawn(alerts.get().list, (id) => pos.get(id)?.fix.point);
      set(SRC.rings, fc(d.rings.map(point)));
      set(SRC.silent, fc(d.silent.map(point)));
      set(SRC.accuracy, fc(d.accuracy));
      ringing = d.rings.length > 0;
      if (ringing && !raf) raf = requestAnimationFrame(animate);
    };

    draw();
    const offAlerts = alerts.subscribe(draw);
    // Positions move every frame at 250 collars; the markers follow twice a second, and only
    // while some alert is about collars.
    let timer: ReturnType<typeof setTimeout> | undefined;
    const offPos = ctx.onPositions(() => {
      if (timer || !alerts.get().list.some((a) => collarsOf(a).length)) return;
      timer = setTimeout(() => {
        timer = undefined;
        draw();
      }, 500);
    });
    attach(ctx);
    return {
      update: draw,
      destroy() {
        detach();
        offAlerts();
        offPos();
        clearTimeout(timer);
        cancelAnimationFrame(raf);
        for (const id of ["alert-rings", "alert-silent", "alert-accuracy-line", "alert-accuracy-fill"]) if (map.getLayer(id)) map.removeLayer(id);
        for (const id of Object.values(SRC)) if (map.getSource(id)) map.removeSource(id);
      },
    };
  },
};
