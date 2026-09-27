import * as maplibregl from "maplibre-gl";
import type { GeoJSONSource, Map as MLMap } from "maplibre-gl";
import type { CollarState, LonLat } from "../api";
import { ANIMAL_SIZE, AnimalsModel, EASE_MIN_ZOOM, OUTLINE_MAX_ZOOM, frameDue, type AnimalIn } from "./animals-model";
import { C, fc } from "./base";

// Animals as small pixel squares. Each fix eases the square from where it is to the new
// point, so the herd drifts instead of jumping; below z15, in a hidden tab or while
// scrubbing a replay they jump. Only what changed goes to the map (source diffs, at most
// 30 frames a second), so 250 animals stay smooth. The name shows on hover only.

const PR = 2; // device pixels per css pixel in the icon bitmap

function square(fill: string, size: number, ring = false): { width: number; height: number; data: Uint8Array } {
  const n = size * PR;
  const data = new Uint8Array(n * n * 4);
  const hex = (h: string) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
  const [r, g, b] = hex(fill);
  const [ir, ig, ib] = hex(C.ink);
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++) {
      const edge = x < PR || y < PR || x >= n - PR || y >= n - PR;
      const o = (y * n + x) * 4;
      if (ring) {
        if (!edge) continue;
        data.set([r, g, b, 255], o);
      } else data.set(edge ? [ir, ig, ib, 200] : [r, g, b, 255], o);
    }
  return { width: n, height: n, data };
}

const rgba = (hex: string, a: number) => `rgba(${[1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16)).join(",")},${a})`;
const size = ANIMAL_SIZE as unknown as maplibregl.ExpressionSpecification;

export class Animals {
  private m = new AnimalsModel();
  private raf = 0;
  private last = 0;
  private hovered?: string;
  private label?: maplibregl.Marker;
  private onZoom = () => this.outline();

  constructor(private map: MLMap, private labelOf: (id: string) => string | undefined = () => undefined) {
    map.addImage("sq-inside", square(C.grass, 7), { pixelRatio: PR });
    map.addImage("sq-warning", square(C.warn, 7), { pixelRatio: PR });
    map.addImage("sq-outside", square(C.red, 7), { pixelRatio: PR });
    map.addImage("sq-unknown", square(C.fg3, 7), { pixelRatio: PR });
    map.addImage("sq-ring", square(C.fg, 13, true), { pixelRatio: PR });
    map.addImage("sq-lag", square(C.warn, 13, true), { pixelRatio: PR });
    // Feature ids are collar ids, so each frame sends only a diff.
    map.addSource("animals", { type: "geojson", data: fc([]), promoteId: "id" });
    map.addSource("herd-outline", { type: "geojson", data: fc([]) });
    map.addLayer({
      id: "herd-outline", type: "line", source: "herd-outline", maxzoom: OUTLINE_MAX_ZOOM,
      paint: { "line-color": C.fg, "line-opacity": 0.45, "line-width": 1 },
    });
    // Trails fade from nothing at the tail to faint grass at the animal.
    map.addSource("trails", { type: "geojson", data: fc([]), lineMetrics: true });
    const trail = (id: string, color: string, lag: boolean) => map.addLayer({
      id, type: "line", source: "trails", filter: ["==", ["get", "lag"], lag],
      layout: { "line-cap": "round", "line-join": "round" },
      paint: {
        "line-width": 1.25,
        "line-gradient": ["interpolate", ["linear"], ["line-progress"], 0, rgba(color, 0), 1, rgba(color, 0.5)],
      },
    });
    trail("trails", C.grass, false);
    trail("trails-lag", C.warn, true);
    map.addLayer({
      id: "animals-hi", type: "symbol", source: "animals", filter: ["==", ["get", "id"], ""],
      layout: { "icon-image": "sq-ring", "icon-size": size, "icon-allow-overlap": true, "icon-ignore-placement": true },
    });
    // A straggler: soft orange with a faint ring, whatever its fence state.
    map.addLayer({
      id: "animals-lag", type: "symbol", source: "animals", filter: ["==", ["get", "lag"], true],
      layout: { "icon-image": "sq-lag", "icon-size": size, "icon-allow-overlap": true, "icon-ignore-placement": true },
      paint: { "icon-opacity": 0.55 },
    });
    map.addLayer({
      id: "animals", type: "symbol", source: "animals",
      layout: {
        "icon-image": ["case", ["get", "lag"], "sq-warning", ["concat", "sq-", ["get", "state"]]],
        "icon-size": size,
        "icon-allow-overlap": true,
        "icon-ignore-placement": true,
      },
    });
    map.on("zoomend", this.onZoom);
  }

  // Easing is only worth drawing close in and when someone is looking.
  private easing() {
    return this.map.getZoom() >= EASE_MIN_ZOOM && !document.hidden;
  }

  // Who is drawn: membership and positions from the collar list.
  set(items: readonly AnimalIn[]) {
    this.m.set(items, performance.now(), this.easing());
    this.loop();
  }

  // Positions from the live feed or a replay; `jump` skips easing (scrubbing).
  moveMany(items: readonly { id: string; point: LonLat; state: CollarState; herd?: string }[], jump = false) {
    const now = performance.now();
    const easing = !jump && this.easing();
    for (const it of items) this.m.move(it.id, it.point, it.state, now, easing, it.herd);
    this.loop();
  }

  move(id: string, to: LonLat, state: CollarState) {
    this.moveMany([{ id, point: to, state }]);
  }

  stragglers(ids: string[]) {
    this.m.setLag(ids);
    this.loop();
  }

  // Animals whose trails show in a big herd besides stragglers: rung, hovered, selected.
  focus(ids: string[]) {
    this.m.setFocus(ids);
    this.loop();
  }

  showTrails(on: boolean) {
    this.m.showTrails(on);
    this.loop();
  }

  // Ring one animal and put its name by it; nothing shows otherwise.
  highlight(id?: string) {
    this.hovered = id;
    if (this.map.getLayer("animals-hi")) this.map.setFilter("animals-hi", ["==", ["get", "id"], id ?? ""]);
    const at = id ? this.m.drawnAt(id, performance.now()) : undefined;
    const text = id ? this.labelOf(id) : undefined;
    if (!at || !text) {
      this.label?.remove();
      this.label = undefined;
      return;
    }
    if (!this.label) {
      const el = document.createElement("div");
      el.className = "plabel";
      this.label = new maplibregl.Marker({ element: el, anchor: "left", offset: [9, 0] }).setLngLat(at).addTo(this.map);
    }
    this.label.setLngLat(at);
    this.label.getElement().textContent = text;
  }

  where(id: string): LonLat | undefined {
    return this.m.where(id);
  }

  private loop() {
    if (this.raf) return;
    const frame = (now: number) => {
      this.raf = 0;
      if (!frameDue(this.last, now)) {
        this.raf = requestAnimationFrame(frame);
        return;
      }
      this.last = now;
      if (this.draw(now)) this.raf = requestAnimationFrame(frame);
    };
    this.raf = requestAnimationFrame(frame);
  }

  // Returns whether any square is still easing.
  private draw(now: number) {
    const src = this.map.getSource("animals") as GeoJSONSource | undefined;
    if (!src) return false;
    const { diff, moving } = this.m.frame(now);
    if (diff) void src.updateData(diff as maplibregl.GeoJSONSourceDiff);
    const trails = this.m.trails(now);
    if (trails) {
      (this.map.getSource("trails") as GeoJSONSource | undefined)?.setData(
        fc(trails.map((t) => ({ type: "Feature", properties: { lag: t.lag }, geometry: { type: "LineString", coordinates: t.path } }))),
      );
    }
    if (this.hovered && diff) {
      const at = this.m.drawnAt(this.hovered, now);
      if (at) this.label?.setLngLat(at);
    }
    this.outline();
    return moving;
  }

  // The herd's outline, only while zoomed out far enough to see it.
  private outline() {
    if (this.map.getZoom() >= OUTLINE_MAX_ZOOM) return;
    const rings = this.m.outlines();
    if (!rings) return;
    (this.map.getSource("herd-outline") as GeoJSONSource | undefined)?.setData(
      fc(rings.map((r) => ({ type: "Feature", properties: { herd: r.herd }, geometry: { type: "Polygon", coordinates: [r.ring] } }))),
    );
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    this.map.off("zoomend", this.onZoom);
    this.label?.remove();
  }
}
