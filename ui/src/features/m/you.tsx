// The "you" dot and the line to the picked animal, on touch screens. Locate (bottom right, above the
// sheet) asks for your position and follows it; with an animal picked, one mono line at the bottom
// reads "214  340 m NE" in the farm's units. The pick is rung on the map; the line opens its page.
// Also here: the map keeps its centre above the sheet, and animals dim while the server is out of
// reach (their last known places).

import { useEffect, useReducer } from "react";
import { createRoot } from "react-dom/client";
import type { GeoJSONSource, Map as MLMap } from "maplibre-gl";
import type { LonLat } from "../../api";
import { C, fc, setData } from "../../map/base";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { views } from "../../registry";
import { collarLabels, store, useStore } from "../../store";
import { picked, sheet, you } from "../../store/m";
import { useUnits } from "../../units";
import { pageHash, pageKeys } from "../k-animals/herd";
import { walkLine } from "./geo";
import { useTouchUI } from "./phone";

const SRC = "m-you";
const LAYERS = ["m-you-acc", "m-you-dot"];
const PR = 2;

// An 11 px square in fg with an ink edge and a blaze centre: not an animal.
function youImage() {
  const n = 11 * PR;
  const data = new Uint8Array(n * n * 4);
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++) {
      const edge = x < PR || y < PR || x >= n - PR || y >= n - PR;
      const mid = x >= 4 * PR && x < 7 * PR && y >= 4 * PR && y < 7 * PR;
      data.set(edge ? [12, 22, 6, 230] : mid ? [255, 106, 43, 255] : [243, 242, 234, 255], (y * n + x) * 4);
    }
  return { width: n, height: n, data };
}

// A circle of `r` metres round `c`, as a polygon.
function circle(c: LonLat, r: number): GeoJSON.Polygon {
  const k = 111_320;
  const pts: LonLat[] = [];
  for (let i = 0; i <= 48; i++) {
    const a = (i / 48) * 2 * Math.PI;
    pts.push([c[0] + (r * Math.cos(a)) / (k * Math.cos((c[1] * Math.PI) / 180)), c[1] + (r * Math.sin(a)) / k]);
  }
  return { type: "Polygon", coordinates: [pts] };
}

let watchId: number | undefined;

// Start or stop following your position. The first fix shows you (and the picked animal).
export function toggleYou(map: MLMap, ctx: OverlayCtx) {
  if (watchId !== undefined) {
    navigator.geolocation.clearWatch(watchId);
    watchId = undefined;
    you.set({ on: false });
    return;
  }
  let first = true;
  you.set({ on: true });
  watchId = navigator.geolocation.watchPosition(
    (p) => {
      const at: LonLat = [p.coords.longitude, p.coords.latitude];
      you.set({ on: true, at, accuracy_m: p.coords.accuracy });
      if (!first) return;
      first = false;
      const id = picked.get();
      const to = id ? ctx.positions().get(id)?.fix.point : undefined;
      if (!to) return void map.easeTo({ center: at, duration: 700 });
      const lon = [at[0], to[0]], lat = [at[1], to[1]];
      // Clear of the top bar's tools and of the Layers row, the line and the time rail above the sheet.
      map.fitBounds([[Math.min(...lon), Math.min(...lat)], [Math.max(...lon), Math.max(...lat)]], { padding: { top: 90, left: 50, right: 50, bottom: 150 }, maxZoom: 18, duration: 700 });
    },
    (e) => {
      if (watchId !== undefined) navigator.geolocation.clearWatch(watchId);
      watchId = undefined;
      you.set({ on: false, error: e.code === e.PERMISSION_DENIED ? "Location is off for this site." : "Can't find where you are." });
      setTimeout(() => you.get().error && you.set({ on: false }), 5000);
    },
    { enableHighAccuracy: true, maximumAge: 5000, timeout: 30_000 },
  );
}

const canLocate = () => typeof navigator !== "undefined" && "geolocation" in navigator && window.isSecureContext;

// A 7×7 pixel crosshair, like the other pixel marks.
const CROSS = ["...#...", ".#####.", ".#...#.", "##.#.##", ".#...#.", ".#####.", "...#..."];
function Cross() {
  return (
    <svg className="px" width={16} height={16} viewBox="0 0 7 7" aria-hidden shapeRendering="crispEdges">
      {CROSS.flatMap((row, y) => [...row].map((c, x) => (c === "#" ? <rect key={`${x},${y}`} x={x} y={y} width={1} height={1} /> : null)))}
    </svg>
  );
}

function Controls({ ctx }: { ctx: OverlayCtx }) {
  const touch = useTouchUI();
  const y = you.use((v) => v);
  const pick = picked.use((v) => v);
  const collars = useStore((s) => s.collars);
  const animals = useStore((s) => s.animals);
  const u = useUnits();
  const [, redraw] = useReducer((n: number) => n + 1, 0);
  // The line follows the animal.
  useEffect(() => (pick ? ctx.onPositions((ids) => ids.includes(pick) && redraw()) : undefined), [ctx, pick]);
  if (!touch) return null;
  const label = pick ? collarLabels(collars, animals).get(pick) : undefined;
  const at = pick ? ctx.positions().get(pick)?.fix.point : undefined;
  const open = () => {
    const c = collars.find((x) => x.id === pick);
    if (!c || !views.has("herd")) return;
    location.hash = pageHash(pageKeys(animals)(animals.find((a) => a.id === c.animal_id || a.collar_id === c.id), c));
  };
  return (
    <>
      {canLocate() && (
        <button type="button" className="mlocate" aria-pressed={y.on} aria-label={y.on ? "Stop showing where you are" : "Show where you are"}
          onClick={() => toggleYou(ctx.map, ctx)}>
          <Cross />
        </button>
      )}
      {y.error ? (
        <p className="mline mono err" role="status">{y.error}</p>
      ) : label && (
        <button type="button" className="mline mono" onClick={open}>{walkLine(label, y.at, at, u.len)}</button>
      )}
    </>
  );
}

export function mountYou(ctx: OverlayCtx): OverlayHandle {
  const map = ctx.map;
  setData(map, SRC, fc([]));
  if (!map.hasImage("m-you")) map.addImage("m-you", youImage(), { pixelRatio: PR });
  const before = map.getLayer(ctx.beforeId("slot-top")) ? ctx.beforeId("slot-top") : undefined;
  map.addLayer({ id: LAYERS[0], type: "fill", source: SRC, filter: ["==", ["geometry-type"], "Polygon"], paint: { "fill-color": C.fg, "fill-opacity": 0.07 } }, before);
  map.addLayer({
    id: LAYERS[1], type: "symbol", source: SRC, filter: ["==", ["geometry-type"], "Point"],
    layout: { "icon-image": "m-you", "icon-allow-overlap": true, "icon-ignore-placement": true },
  }, before);

  const draw = () => {
    const y = you.get();
    const f: GeoJSON.Feature[] = [];
    if (y.on && y.at) {
      if (y.accuracy_m && y.accuracy_m > 3) f.push({ type: "Feature", properties: {}, geometry: circle(y.at, Math.min(y.accuracy_m, 500)) });
      f.push({ type: "Feature", properties: {}, geometry: { type: "Point", coordinates: y.at } });
    }
    (map.getSource(SRC) as GeoJSONSource | undefined)?.setData(fc(f));
  };

  // The pick is rung; a pick that left the map (parked, another herd hidden) clears.
  const ring = () => {
    const id = picked.get();
    ctx.highlight(id && ctx.positions().has(id) ? [id] : [], "fg");
  };

  // The camera keeps what it centres on above the sheet (at most half the map).
  let pad = -1;
  const padding = () => {
    const h = sheet.get().h;
    const bottom = Math.min(h, Math.round(map.getContainer().clientHeight * 0.5));
    if (bottom === pad) return;
    pad = bottom;
    map.setPadding({ top: 0, left: 0, right: 0, bottom });
  };

  // Out of reach: the animals stay at their last known places, dimmed.
  let dim: boolean | undefined;
  const offline = () => {
    const s = store.get();
    const d = !s.up && !!s.state;
    if (d === dim || !map.getLayer("animals")) return;
    dim = d;
    map.setPaintProperty("animals", "icon-opacity", d ? 0.45 : 1);
  };

  const el = document.createElement("div");
  el.className = "mhost";
  map.getContainer().appendChild(el);
  const root = createRoot(el);
  root.render(<Controls ctx={ctx} />);

  const offs = [you.subscribe(draw), picked.subscribe(ring), sheet.subscribe(padding), store.subscribe(offline), ctx.onPositions(() => picked.get() && ring())];
  draw();
  ring();
  padding();
  offline();
  return {
    update: ring,
    destroy() {
      offs.forEach((f) => f());
      root.unmount();
      el.remove();
      try {
        map.setPadding({ top: 0, left: 0, right: 0, bottom: 0 });
        for (const id of [...LAYERS].reverse()) if (map.getLayer(id)) map.removeLayer(id);
        if (map.getSource(SRC)) map.removeSource(SRC);
      } catch {
        /* the map went first */
      }
    },
  };
}
