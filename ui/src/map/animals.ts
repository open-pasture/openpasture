import type { Map as MLMap } from "maplibre-gl";
import type { CollarState, LonLat } from "../api";
import { C, fc, setData } from "./base";

// Animals as small pixel squares. Each fix eases the square from where it is
// to the new point, so the herd drifts instead of jumping.

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

interface Anim { from: LonLat; to: LonLat; t0: number; state: CollarState; trail: { p: LonLat; t: number }[] }
// A move draws where each animal walked in the last TRAIL ms.
const TRAIL = 75_000;
const DUR = 950;
const ease = (t: number) => 1 - Math.pow(1 - t, 2);

export class Animals {
  private a = new Map<string, Anim>();
  private raf = 0;
  private lag = new Set<string>(); // stragglers of a running move
  private trails = false;

  constructor(private map: MLMap) {
    map.addImage("sq-inside", square(C.grass, 7), { pixelRatio: PR });
    map.addImage("sq-warning", square(C.warn, 7), { pixelRatio: PR });
    map.addImage("sq-outside", square(C.red, 7), { pixelRatio: PR });
    map.addImage("sq-unknown", square(C.fg3, 7), { pixelRatio: PR });
    map.addImage("sq-ring", square(C.fg, 13, true), { pixelRatio: PR });
    map.addImage("sq-lag", square(C.warn, 13, true), { pixelRatio: PR });
    setData(map, "animals", fc([]));
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
      layout: { "icon-image": "sq-ring", "icon-allow-overlap": true, "icon-ignore-placement": true },
    });
    // A straggler: soft orange with a faint ring, whatever its fence state.
    map.addLayer({
      id: "animals-lag", type: "symbol", source: "animals", filter: ["==", ["get", "lag"], true],
      layout: { "icon-image": "sq-lag", "icon-allow-overlap": true, "icon-ignore-placement": true },
      paint: { "icon-opacity": 0.55 },
    });
    map.addLayer({
      id: "animals", type: "symbol", source: "animals",
      layout: {
        "icon-image": ["case", ["get", "lag"], "sq-warning", ["concat", "sq-", ["get", "state"]]],
        "icon-allow-overlap": true,
        "icon-ignore-placement": true,
      },
    });
  }

  set(items: { id: string; point: LonLat; state: CollarState }[]) {
    const now = performance.now();
    const keep = new Set(items.map((i) => i.id));
    for (const id of this.a.keys()) if (!keep.has(id)) this.a.delete(id);
    for (const it of items) {
      const cur = this.a.get(it.id);
      if (!cur) this.a.set(it.id, { from: it.point, to: it.point, t0: now - DUR, state: it.state, trail: [] });
      else this.move(it.id, it.point, it.state);
    }
    this.loop();
  }

  move(id: string, to: LonLat, state: CollarState) {
    const now = performance.now();
    const cur = this.a.get(id);
    const from = cur ? this.at(cur, now) : to;
    const trail = (cur?.trail ?? []).filter((x) => now - x.t < TRAIL);
    if (cur) trail.push({ p: cur.to, t: now });
    this.a.set(id, { from, to, t0: now, state, trail });
    this.loop();
  }

  stragglers(ids: string[]) {
    const next = new Set(ids);
    if (next.size === this.lag.size && ids.every((i) => this.lag.has(i))) return;
    this.lag = next;
    this.draw(performance.now());
  }

  showTrails(on: boolean) {
    if (on === this.trails) return;
    this.trails = on;
    this.draw(performance.now());
  }

  highlight(id?: string) {
    if (this.map.getLayer("animals-hi")) this.map.setFilter("animals-hi", ["==", ["get", "id"], id ?? ""]);
  }

  where(id: string): LonLat | undefined {
    return this.a.get(id)?.to;
  }

  private at(a: Anim, now: number): LonLat {
    const t = ease(Math.min(1, (now - a.t0) / DUR));
    return [a.from[0] + (a.to[0] - a.from[0]) * t, a.from[1] + (a.to[1] - a.from[1]) * t];
  }

  private loop() {
    if (this.raf) return;
    const frame = () => {
      this.raf = this.draw(performance.now()) ? requestAnimationFrame(frame) : 0;
    };
    this.raf = requestAnimationFrame(frame);
  }

  // Returns whether any square is still easing.
  private draw(now: number) {
    let moving = false;
    const feats: GeoJSON.Feature[] = [];
    const trails: GeoJSON.Feature[] = [];
    for (const [id, a] of this.a) {
      if (now - a.t0 < DUR) moving = true;
      const at = this.at(a, now);
      const lag = this.lag.has(id);
      feats.push({ type: "Feature", properties: { id, state: a.state, lag }, geometry: { type: "Point", coordinates: at } });
      if (this.trails) {
        // Averaged over three fixes, so GPS jitter and grazing steps don't scribble.
        const raw = [...a.trail.filter((x) => now - x.t < TRAIL).map((x) => x.p), at];
        const path = raw.map((_, i) => {
          const w = raw.slice(Math.max(0, i - 1), i + 2);
          return i === raw.length - 1 ? at : ([w.reduce((s, p) => s + p[0], 0) / w.length, w.reduce((s, p) => s + p[1], 0) / w.length] as LonLat);
        });
        if (path.length > 1) trails.push({ type: "Feature", properties: { lag }, geometry: { type: "LineString", coordinates: path } });
      }
    }
    setData(this.map, "animals", fc(feats));
    setData(this.map, "trails", fc(trails));
    return moving;
  }

  destroy() {
    cancelAnimationFrame(this.raf);
  }
}
